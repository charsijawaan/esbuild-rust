use std::{
    collections::{HashMap, HashSet},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use crate::internal::{
    ast::{
        CharFreq, INVALID_REF, ImportKind, ImportRecord, ImportRecordFlags, Index32, LocRef, Ref,
        SlotNamespace, Symbol, SymbolKind,
    },
    helpers::{string_to_utf16, utf16_to_string},
    js_ast::{
        Ast, Binding, BindingData, CallExpr, CallKind, ClauseItem, Decl, DeclaredSymbol, DotExpr,
        ExportsKind, Expr, ExprData, ExprStmt, IdentifierBinding, IdentifierExpr, ImportStmt,
        LazyExportStmt, LocalKind, LocalStmt, NamedExport, NamedImport, Part, Scope, ScopeKind,
        ScopeRef, Stmt, StmtData, StmtsCanBeRemovedIfUnusedFlags, StrictModeKind, StringExpr,
        for_each_identifier_binding, make_helper_context,
    },
    js_lexer::{Lexer, LexerPanic, Token},
    logger::{Loc, Log, Path, Source},
    runtime,
};

use super::{
    Options,
    lower_typescript::{LowerTypeScriptContext, lower_type_script_statements},
    parser_core::ParserCore,
    parser_types::AwaitOrYield,
    syntax_statement::parse_statements_up_to,
    visit::{
        merge_adjacent_expression_statements, precompute_type_script_enum_constants,
        visit_top_level_statements,
    },
};

const MODULE_SCOPE_LOC: Loc = Loc { start: -1 };

fn compute_character_frequency(core: &ParserCore, lexer: &Lexer) -> Option<CharFreq> {
    if !core.options.minify_identifiers || core.source.key_path.text == "<runtime>" {
        return None;
    }
    let mut frequency = CharFreq::default();
    frequency.scan(&core.source.contents, 1);
    for comment in &lexer.all_comments {
        frequency.scan(core.source.text_for_range(*comment), -1);
    }
    for record in &core.import_records {
        if !record.source_index.is_valid() {
            frequency.scan(record.path.text.as_bytes(), -1);
        }
    }
    if let Some(scope) = &core.module_scope {
        subtract_symbol_names_from_frequency(scope, &core.symbols, &mut frequency);
    }
    for reference in core.mangled_props.values() {
        let symbol =
            &core.symbols[usize::try_from(reference.inner_index).expect("symbol index fits usize")];
        frequency.scan(
            symbol.original_name.as_bytes(),
            -i32::try_from(symbol.use_count_estimate).unwrap_or(i32::MAX),
        );
    }
    Some(frequency)
}

fn subtract_symbol_names_from_frequency(
    scope: &ScopeRef,
    symbols: &[Symbol],
    frequency: &mut CharFreq,
) {
    let scope = scope
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for member in scope.members.values() {
        let symbol = &symbols
            [usize::try_from(member.reference.inner_index).expect("symbol index fits usize")];
        if symbol.slot_namespace() != SlotNamespace::MustNotBeRenamed {
            frequency.scan(
                symbol.original_name.as_bytes(),
                -i32::try_from(symbol.use_count_estimate).unwrap_or(i32::MAX),
            );
        }
    }
    if scope.label.reference != INVALID_REF {
        let symbol = &symbols
            [usize::try_from(scope.label.reference.inner_index).expect("symbol index fits usize")];
        if symbol.slot_namespace() != SlotNamespace::MustNotBeRenamed {
            let count = i32::try_from(symbol.use_count_estimate)
                .unwrap_or(i32::MAX)
                .saturating_add(1);
            frequency.scan(symbol.original_name.as_bytes(), -count);
        }
    }
    for child in &scope.children {
        subtract_symbol_names_from_frequency(child, symbols, frequency);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HelperCall {
    pub global: Vec<String>,
    pub runtime: String,
}

/// Construct an AST whose default export is generated lazily during linking.
///
/// # Panics
///
/// Panics if parser scope invariants are violated or generated import and
/// symbol indexes do not fit in their upstream-compatible integer widths.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn lazy_export_ast(
    log: Log,
    source: &Source,
    options: Options,
    mut expression: Expr,
    helper_call: Option<&HelperCall>,
) -> Ast {
    let approximate_line_count =
        i32::try_from(source.contents.split(|byte| *byte == b'\n').count()).unwrap_or(i32::MAX);
    let mut core = ParserCore::new_with_log(source.clone(), options, log);
    core.push_scope_for_parse_pass(ScopeKind::Entry, MODULE_SCOPE_LOC);
    core.prepare_for_visit_pass(false, false);

    if let Some(helper_call) = helper_call {
        core.symbol_uses = HashMap::new();
        if let Some(first) = helper_call.global.first() {
            let reference = core.new_symbol(SymbolKind::Unbound, first.clone());
            core.record_usage(reference);
            let mut target = Expr::new(
                expression.loc,
                ExprData::Identifier(IdentifierExpr {
                    reference,
                    ..IdentifierExpr::default()
                }),
            );
            let mut kind = CallKind::Normal;
            for name in helper_call.global.iter().skip(1) {
                target = Expr::new(
                    expression.loc,
                    ExprData::Dot(DotExpr {
                        target,
                        name: name.clone(),
                        ..DotExpr::default()
                    }),
                );
                kind = CallKind::TargetWasOriginallyPropertyAccess;
            }
            expression = Expr::new(
                expression.loc,
                ExprData::Call(CallExpr {
                    target,
                    args: vec![expression],
                    kind,
                    ..CallExpr::default()
                }),
            );
        } else if !helper_call.runtime.is_empty() {
            expression = core.call_runtime(expression.loc, &helper_call.runtime, vec![expression]);
        }
    }

    let namespace_export_part = Part {
        symbol_uses: HashMap::new(),
        can_be_removed_if_unused: true,
        ..Part::default()
    };
    let lazy_export_part = Part {
        statements: vec![Stmt::new(
            expression.loc,
            StmtData::LazyExport(LazyExportStmt { value: expression }),
        )],
        symbol_uses: std::mem::take(&mut core.symbol_uses),
        ..Part::default()
    };
    let mut parts = vec![namespace_export_part];
    let mut named_imports = HashMap::new();

    if !core.runtime_imports.is_empty() && !core.options.omit_runtime_for_tests {
        let mut imports: Vec<(String, LocRef)> = core.runtime_imports.drain().collect();
        imports.sort_by(|left, right| left.0.cmp(&right.0));
        let namespace_ref = core.new_symbol(SymbolKind::Other, "import_runtime");
        core.module_scope
            .as_ref()
            .expect("runtime imports require a module scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generated
            .push(namespace_ref);
        let import_record_index =
            u32::try_from(core.import_records.len()).expect("import record count fits in u32");
        core.import_records.push(ImportRecord {
            path: Path {
                text: "<runtime>".into(),
                namespace: "file".into(),
                ..Path::default()
            },
            source_index: Index32::new(runtime::SOURCE_INDEX),
            kind: ImportKind::Stmt,
            ..ImportRecord::default()
        });
        let mut declared_symbols = vec![DeclaredSymbol {
            reference: namespace_ref,
            is_top_level: true,
        }];
        let mut items = Vec::with_capacity(imports.len());
        for (alias, item) in imports {
            declared_symbols.push(DeclaredSymbol {
                reference: item.reference,
                is_top_level: true,
            });
            items.push(ClauseItem {
                alias: alias.clone(),
                alias_loc: item.loc,
                name: item,
                ..ClauseItem::default()
            });
            named_imports.insert(
                item.reference,
                NamedImport {
                    alias,
                    alias_loc: item.loc,
                    namespace_ref,
                    import_record_index,
                    ..NamedImport::default()
                },
            );
        }
        parts.push(Part {
            statements: vec![Stmt::new(
                Loc::default(),
                StmtData::Import(ImportStmt {
                    items: Some(items),
                    namespace_ref,
                    import_record_index,
                    is_single_line: true,
                    ..ImportStmt::default()
                }),
            )],
            import_record_indices: vec![import_record_index],
            declared_symbols,
            ..Part::default()
        });
    }
    parts.push(lazy_export_part);

    let wrapper_ref = core.new_symbol(
        SymbolKind::Other,
        format!("require_{}", source.identifier_name),
    );
    let mut top_level_symbol_to_parts_from_parser = HashMap::new();
    top_level_symbol_to_parts_from_parser.insert(core.exports_ref, vec![0]);
    let exports_kind = if core.options.module_type_data.module_type.is_esm() {
        ExportsKind::Esm
    } else if core.options.module_type_data.module_type.is_common_js() {
        ExportsKind::CommonJs
    } else {
        ExportsKind::None
    };

    Ast {
        module_type_data: core.options.module_type_data,
        parts,
        symbols: core.symbols,
        expr_comments: core.expr_comments,
        module_scope: core.module_scope,
        top_level_symbol_to_parts_from_parser,
        import_records: core.import_records,
        named_imports,
        exports_ref: core.exports_ref,
        module_ref: core.module_ref,
        wrapper_ref,
        approximate_line_count,
        exports_kind,
        has_lazy_export: true,
        const_values: core.const_values,
        mangled_props: core.mangled_props,
        reserved_props: core.reserved_props,
        ..Ast::default()
    }
}

/// Parse a JavaScript or TypeScript source file into esbuild's JavaScript AST.
///
/// This follows upstream's two-value error convention: syntax errors raised by
/// the lexer are logged and return `false`, while internal panics continue
/// unwinding.
///
/// # Panics
///
/// Panics if parser invariants are violated. Syntax errors from the lexer are
/// caught and reported through the returned boolean instead.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn parse(log: Log, source: Source, options: Options) -> (Ast, bool) {
    let mut result = Ast::default();
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        let mut options = options;
        if options.jsx.factory.parts.is_empty() {
            options.jsx.factory.parts = vec!["React".into(), "createElement".into()];
        }
        if options.jsx.fragment.parts.is_empty() && options.jsx.fragment.constant.data.is_none() {
            options.jsx.fragment.parts = vec!["React".into(), "Fragment".into()];
        }
        if options.jsx.import_source.is_empty() {
            options.jsx.import_source = "react".into();
        }
        if options.defines.is_none() {
            options.defines = Some(Arc::new(crate::internal::config::process_defines(&[])));
        }
        let mut lexer = Lexer::new(log.clone(), source.clone(), options.ts.clone());
        let mut core = ParserCore::new_with_log(source, options, log);
        core.push_scope_for_parse_pass(ScopeKind::Entry, MODULE_SCOPE_LOC);

        let hashbang = if lexer.token == Token::Hashbang {
            let value = String::from_utf8_lossy(&lexer.identifier.string).into_owned();
            lexer.next();
            value
        } else {
            String::new()
        };

        // Top-level await is syntactically allowed. The visit pass later
        // determines whether the file is an ECMAScript module.
        core.fn_or_arrow_data_parse.await_policy = AwaitOrYield::AllowExpression;

        let mut statements = parse_statements_up_to(&mut core, &mut lexer, Token::EndOfFile);
        apply_jsx_pragmas(&mut core, &lexer);

        let (directives, directive_legacy_octal_locs) =
            strip_directive_prologue(&core, &mut statements);
        if directives.iter().any(|directive| directive == "use strict") {
            Scope::recursive_set_strict_mode(
                core.current_scope
                    .as_ref()
                    .expect("directive prologue requires an entry scope"),
                StrictModeKind::ExplicitStrict,
            );
        }
        let has_import_statement = statements
            .iter()
            .any(|statement| matches!(statement.data.as_deref(), Some(StmtData::Import(_))));
        let has_esm_exports = core.esm_import_meta.len > 0
            || core.top_level_await_keyword.len > 0
            || (core.options.jsx.automatic_runtime && core.has_jsx_element)
            || core.has_type_script_export
            || statements.iter().any(|statement| {
                matches!(
                    statement.data.as_deref(),
                    Some(
                        StmtData::ExportClause(_)
                            | StmtData::ExportFrom(_)
                            | StmtData::ExportDefault(_)
                            | StmtData::ExportStar(_)
                    )
                ) || matches!(
                    statement.data.as_deref(),
                    Some(StmtData::Local(local)) if local.is_export
                ) || matches!(
                    statement.data.as_deref(),
                    Some(StmtData::Function(function)) if function.is_export
                ) || matches!(
                    statement.data.as_deref(),
                    Some(StmtData::Class(class)) if class.is_export
                ) || matches!(
                    statement.data.as_deref(),
                    Some(StmtData::Enum(enumeration)) if enumeration.is_export
                ) || matches!(
                    statement.data.as_deref(),
                    Some(StmtData::Namespace(namespace)) if namespace.is_export
                )
            });
        if has_esm_exports
            || has_import_statement
            || core.options.module_type_data.module_type.is_esm()
            || directives.iter().any(|directive| directive == "use strict")
        {
            for loc in directive_legacy_octal_locs {
                core.add_error_range(
                    core.source.range_of_legacy_octal_escape(loc),
                    "Legacy octal escape sequences cannot be used in strict mode",
                );
            }
        }
        if core.options.tree_shaking {
            statements = split_top_level_local_statements(statements);
        }
        let mut declared_symbols_by_statement = Vec::with_capacity(statements.len());
        for statement in &mut statements {
            declared_symbols_by_statement.push(declare_top_level_symbols(
                &mut core,
                std::slice::from_mut(statement),
            ));
        }
        core.declared_symbols.clear();
        if has_esm_exports
            || has_import_statement
            || core.options.module_type_data.module_type.is_esm()
        {
            Scope::recursive_set_strict_mode(
                core.current_scope
                    .as_ref()
                    .expect("parse pass requires a module scope"),
                StrictModeKind::ImplicitStrictEsm,
            );
        }
        core.hoist_symbols();
        let scopes = core.scope_refs_in_order();
        core.prepare_for_visit_pass(has_esm_exports, has_import_statement);
        precompute_type_script_enum_constants(&mut core, &statements);
        let (mut parts, mut module_metadata, uses_exports_ref, uses_module_ref) =
            if core.options.tree_shaking {
                build_tree_shaking_parts(&mut core, statements, declared_symbols_by_statement)
            } else {
                core.declared_symbols = declared_symbols_by_statement.into_iter().flatten().fold(
                    Vec::new(),
                    |mut symbols, symbol| {
                        record_top_level_symbol(&mut symbols, symbol.reference);
                        symbols
                    },
                );
                visit_top_level_statements(&mut core, &mut statements);
                statements = lower_type_script_statements(&mut core, statements);
                if core.options.keep_names && core.source.key_path.text != "<runtime>" {
                    apply_keep_names_to_statements(&mut core, &mut statements);
                }
                let uses_exports_ref = core
                    .symbol_uses
                    .get(&core.exports_ref)
                    .is_some_and(|usage| usage.count_estimate > 0);
                let uses_module_ref = core
                    .symbol_uses
                    .get(&core.module_ref)
                    .is_some_and(|usage| usage.count_estimate > 0);
                let module_metadata = scan_module_metadata(&mut core, &mut statements);
                prepend_generated_namespace_import_declarations(
                    &mut core,
                    &module_metadata.named_imports,
                );
                let mut parts = vec![Part {
                    symbol_uses: HashMap::new(),
                    can_be_removed_if_unused: true,
                    ..Part::default()
                }];
                if !statements.is_empty() {
                    let import_record_indices = (0..core.import_records.len())
                        .filter(|index| !is_generated_import_record(&core, *index))
                        .map(|index| u32::try_from(index).expect("import record count fits in u32"))
                        .collect();
                    parts.push(Part {
                        statements,
                        scopes,
                        import_record_indices,
                        declared_symbols: std::mem::take(&mut core.declared_symbols),
                        symbol_uses: std::mem::take(&mut core.symbol_uses),
                        import_symbol_property_uses: std::mem::take(
                            &mut core.import_symbol_property_uses,
                        ),
                        ..Part::default()
                    });
                }
                (parts, module_metadata, uses_exports_ref, uses_module_ref)
            };
        if core.options.ts.parse {
            remove_unused_type_script_import_equals(&mut core, &mut parts);
        }
        insert_runtime_import_part(&mut core, &mut module_metadata, &mut parts);
        insert_generated_import_parts(&core, &module_metadata, &mut parts);
        insert_generated_define_parts(&core, &mut parts);
        insert_top_level_temp_part(&core, &mut parts);
        assert_eq!(
            core.remaining_scope_count(),
            0,
            "visit pass must consume every parse-pass scope"
        );
        let module_scope = core
            .module_scope
            .clone()
            .expect("the parser must have an entry scope");

        // This symbol is always present so the linker can wrap this file later.
        let wrapper_ref = core.new_symbol(
            SymbolKind::Other,
            format!("require_{}", core.source.identifier_name),
        );
        core.pop_scope();
        let nested_scope_slot_counts = if core.options.minify_identifiers {
            crate::internal::renamer::assign_nested_scope_slots(&module_scope, &mut core.symbols)
        } else {
            crate::internal::ast::SlotCounts::default()
        };

        let exports_kind = if has_esm_exports {
            ExportsKind::Esm
        } else if core.has_top_level_return || uses_exports_ref || uses_module_ref {
            ExportsKind::CommonJs
        } else if core.options.module_type_data.module_type.is_common_js() {
            ExportsKind::CommonJs
        } else if core.options.module_type_data.module_type.is_esm() || has_import_statement {
            ExportsKind::Esm
        } else {
            ExportsKind::None
        };
        let mut top_level_symbol_to_parts_from_parser = HashMap::new();
        top_level_symbol_to_parts_from_parser.insert(core.exports_ref, vec![0]);
        for (part_index, part) in parts.iter().enumerate() {
            for declared in &part.declared_symbols {
                if declared.is_top_level {
                    let reference = core.follow_symbol_link(declared.reference);
                    top_level_symbol_to_parts_from_parser
                        .entry(reference)
                        .or_insert_with(Vec::new)
                        .push(u32::try_from(part_index).expect("part index fits in u32"));
                }
            }
        }
        let char_freq = compute_character_frequency(&core, &lexer);

        result = Ast {
            module_type_data: core.options.module_type_data,
            parts,
            char_freq,
            symbols: core.symbols,
            expr_comments: core.expr_comments,
            module_scope: Some(module_scope),
            hashbang,
            directives,
            top_level_symbol_to_parts_from_parser,
            ts_enums: core.ts_enums,
            const_values: core.const_values,
            mangled_props: core.mangled_props,
            reserved_props: core.reserved_props,
            import_records: core.import_records,
            named_imports: module_metadata.named_imports,
            named_exports: module_metadata.named_exports,
            export_star_import_records: module_metadata.export_star_import_records,
            source_map_comment: lexer.source_mapping_url.clone(),
            export_keyword: core.esm_export_keyword,
            top_level_await_keyword: core.top_level_await_keyword,
            live_top_level_await_keyword: core.live_top_level_await_keyword,
            exports_ref: core.exports_ref,
            module_ref: core.module_ref,
            wrapper_ref,
            nested_scope_slot_counts,
            approximate_line_count: i32::try_from(lexer.approximate_newline_count)
                .unwrap_or(i32::MAX)
                .saturating_add(1),
            exports_kind,
            uses_exports_ref,
            uses_module_ref,
            ..Ast::default()
        };
    }));

    match parsed {
        Ok(()) => (result, true),
        Err(payload) if payload.downcast_ref::<LexerPanic>().is_some() => (result, false),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[derive(Default)]
struct ModuleMetadata {
    named_imports: HashMap<Ref, NamedImport>,
    named_exports: HashMap<String, NamedExport>,
    export_star_import_records: Vec<u32>,
}

fn insert_generated_import_parts(
    core: &ParserCore,
    metadata: &ModuleMetadata,
    parts: &mut Vec<Part>,
) {
    let mut generated_imports = core
        .jsx_import_records
        .values()
        .chain(core.glob_import_records.values())
        .copied()
        .collect::<Vec<_>>();
    generated_imports.sort_unstable_by_key(|(record_index, _)| *record_index);

    for (offset, (import_record_index, namespace_ref)) in generated_imports.into_iter().enumerate()
    {
        let mut imports = metadata
            .named_imports
            .iter()
            .filter(|(_, import)| {
                import.import_record_index == import_record_index
                    && import.namespace_ref == namespace_ref
            })
            .map(|(reference, import)| (*reference, import))
            .collect::<Vec<_>>();
        imports.sort_unstable_by(|left, right| left.1.alias.cmp(&right.1.alias));

        let mut declared_symbols = Vec::with_capacity(imports.len() + 1);
        declared_symbols.push(DeclaredSymbol {
            reference: namespace_ref,
            is_top_level: true,
        });
        let items = imports
            .into_iter()
            .map(|(reference, import)| {
                declared_symbols.push(DeclaredSymbol {
                    reference,
                    is_top_level: true,
                });
                ClauseItem {
                    alias: import.alias.clone(),
                    alias_loc: import.alias_loc,
                    name: LocRef {
                        loc: import.alias_loc,
                        reference,
                    },
                    ..ClauseItem::default()
                }
            })
            .collect();
        let loc = core.import_records
            [usize::try_from(import_record_index).expect("import record index")]
        .range
        .loc;
        parts.insert(
            1 + offset,
            Part {
                statements: vec![Stmt::new(
                    loc,
                    StmtData::Import(ImportStmt {
                        items: Some(items),
                        namespace_ref,
                        import_record_index,
                        is_single_line: true,
                        ..ImportStmt::default()
                    }),
                )],
                import_record_indices: vec![import_record_index],
                declared_symbols,
                ..Part::default()
            },
        );
    }
}

fn insert_runtime_import_part(
    core: &mut ParserCore,
    metadata: &mut ModuleMetadata,
    parts: &mut Vec<Part>,
) {
    if core.runtime_imports.is_empty() || core.options.omit_runtime_for_tests {
        return;
    }

    let mut imports: Vec<(String, LocRef)> = core.runtime_imports.drain().collect();
    imports.sort_by(|left, right| left.0.cmp(&right.0));
    let namespace_ref = core.new_symbol(SymbolKind::Other, "import_runtime");
    core.module_scope
        .as_ref()
        .expect("runtime imports require a module scope")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .generated
        .push(namespace_ref);
    let import_record_index =
        u32::try_from(core.import_records.len()).expect("import record count fits in u32");
    core.import_records.push(ImportRecord {
        path: Path {
            text: "<runtime>".into(),
            namespace: "file".into(),
            ..Path::default()
        },
        source_index: Index32::new(runtime::SOURCE_INDEX),
        kind: ImportKind::Stmt,
        ..ImportRecord::default()
    });

    let mut declared_symbols = vec![DeclaredSymbol {
        reference: namespace_ref,
        is_top_level: true,
    }];
    let items = imports
        .into_iter()
        .map(|(alias, item)| {
            declared_symbols.push(DeclaredSymbol {
                reference: item.reference,
                is_top_level: true,
            });
            metadata.named_imports.insert(
                item.reference,
                NamedImport {
                    alias: alias.clone(),
                    alias_loc: item.loc,
                    namespace_ref,
                    import_record_index,
                    ..NamedImport::default()
                },
            );
            ClauseItem {
                alias,
                alias_loc: item.loc,
                name: item,
                ..ClauseItem::default()
            }
        })
        .collect();
    parts.insert(
        1,
        Part {
            statements: vec![Stmt::new(
                Loc::default(),
                StmtData::Import(ImportStmt {
                    items: Some(items),
                    namespace_ref,
                    import_record_index,
                    is_single_line: true,
                    ..ImportStmt::default()
                }),
            )],
            import_record_indices: vec![import_record_index],
            declared_symbols,
            ..Part::default()
        },
    );
}

fn insert_generated_define_parts(core: &ParserCore, parts: &mut Vec<Part>) {
    let Some(defines) = core.options.defines.as_ref() else {
        return;
    };
    let mut generated = core
        .generated_injected_defines
        .iter()
        .map(|(index, reference)| (*index, *reference))
        .collect::<Vec<_>>();
    generated.sort_unstable_by_key(|(index, _)| *index);

    for (offset, (index, reference)) in generated.into_iter().enumerate() {
        let injected = &defines.injected_defines[index as usize];
        parts.insert(
            1 + offset,
            Part {
                statements: vec![Stmt::new(
                    Loc::default(),
                    StmtData::Local(LocalStmt {
                        declarations: vec![Decl {
                            binding: Binding {
                                data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                                    reference,
                                }))),
                                ..Binding::default()
                            },
                            value_or_nil: injected.data.clone(),
                        }],
                        kind: LocalKind::Var,
                        ..LocalStmt::default()
                    }),
                )],
                declared_symbols: vec![DeclaredSymbol {
                    reference,
                    is_top_level: true,
                }],
                can_be_removed_if_unused: true,
                ..Part::default()
            },
        );
    }
}

fn insert_top_level_temp_part(core: &ParserCore, parts: &mut Vec<Part>) {
    if core.top_level_temp_refs.is_empty() {
        return;
    }
    let insert_at = parts
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, part)| {
            !part
                .statements
                .iter()
                .all(|statement| matches!(statement.data.as_deref(), Some(StmtData::Import(_))))
        })
        .map_or(parts.len(), |(index, _)| index);
    let declarations = core
        .top_level_temp_refs
        .iter()
        .copied()
        .map(|reference| Decl {
            binding: Binding {
                data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                    reference,
                }))),
                ..Binding::default()
            },
            ..Decl::default()
        })
        .collect();
    let declared_symbols = core
        .top_level_temp_refs
        .iter()
        .copied()
        .map(|reference| DeclaredSymbol {
            reference,
            is_top_level: true,
        })
        .collect();
    parts.insert(
        insert_at,
        Part {
            statements: vec![Stmt::new(
                Loc::default(),
                StmtData::Local(LocalStmt {
                    declarations,
                    kind: LocalKind::Var,
                    ..LocalStmt::default()
                }),
            )],
            declared_symbols,
            ..Part::default()
        },
    );
}

fn is_generated_import_record(core: &ParserCore, index: usize) -> bool {
    core.jsx_import_records
        .values()
        .chain(core.glob_import_records.values())
        .any(|(record_index, _)| {
            usize::try_from(*record_index).expect("import record index") == index
        })
}

fn split_top_level_local_statements(statements: Vec<Stmt>) -> Vec<Stmt> {
    let mut result = Vec::with_capacity(statements.len());
    for statement in statements {
        let Some(StmtData::Local(local)) = statement.data.as_deref() else {
            result.push(statement);
            continue;
        };
        if local.declarations.len() < 2 {
            result.push(statement);
            continue;
        }
        for declaration in &local.declarations {
            let mut split = local.clone();
            split.declarations = vec![declaration.clone()];
            result.push(Stmt::new(statement.loc, StmtData::Local(split)));
        }
    }
    result
}

fn append_relocated_top_level_vars(core: &mut ParserCore, statements: &mut Vec<Stmt>) {
    if core.relocated_top_level_vars.is_empty() {
        return;
    }
    let mut already_declared = HashSet::new();
    let mut declarations = Vec::new();
    for local in std::mem::take(&mut core.relocated_top_level_vars) {
        let reference = core.follow_symbol_link(local.reference);
        if already_declared.insert(reference) {
            declarations.push(Decl {
                binding: Binding {
                    loc: local.loc,
                    data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                        reference,
                    }))),
                },
                ..Decl::default()
            });
        }
    }
    if !declarations.is_empty() {
        statements.push(Stmt::new(
            declarations[0].binding.loc,
            StmtData::Local(LocalStmt {
                declarations,
                kind: LocalKind::Var,
                ..LocalStmt::default()
            }),
        ));
    }
}

#[allow(clippy::too_many_lines)]
fn build_tree_shaking_parts(
    core: &mut ParserCore,
    statements: Vec<Stmt>,
    declared_symbols_by_statement: Vec<Vec<DeclaredSymbol>>,
) -> (Vec<Part>, ModuleMetadata, bool, bool) {
    let mut before_parts = Vec::new();
    let mut parts = Vec::new();
    let mut after_parts = Vec::new();
    let mut metadata = ModuleMetadata::default();
    let mut lower_context = LowerTypeScriptContext::default();
    let mut uses_exports_ref = false;
    let mut uses_module_ref = false;

    core.scopes_for_current_part.clear();
    for (statement, declared_symbols) in statements.into_iter().zip(declared_symbols_by_statement) {
        let move_before = core.options.mode != crate::internal::config::Mode::PassThrough
            && matches!(
                statement.data.as_deref(),
                Some(StmtData::Import(_) | StmtData::ExportFrom(_) | StmtData::ExportStar(_))
            );
        let move_after = matches!(statement.data.as_deref(), Some(StmtData::ExportEquals(_)));
        core.symbol_uses.clear();
        core.import_symbol_property_uses.clear();
        core.declared_symbols = declared_symbols;
        core.scopes_for_current_part.clear();

        let mut import_record_indices = top_level_import_record_indices(&statement);
        let first_generated_import_record = core.import_records.len();
        let mut statements = vec![statement];
        visit_top_level_statements(core, &mut statements);
        let mut statements = lower_context.lower_statements(core, statements);
        append_relocated_top_level_vars(core, &mut statements);
        if core.options.keep_names && core.source.key_path.text != "<runtime>" {
            apply_keep_names_to_statements(core, &mut statements);
        }
        let defer_import_scan = move_before
            && statements
                .iter()
                .any(|statement| matches!(statement.data.as_deref(), Some(StmtData::Import(_))));
        if !defer_import_scan {
            scan_module_metadata_into(core, &mut statements, &mut metadata);
        }

        uses_exports_ref |= core
            .symbol_uses
            .get(&core.exports_ref)
            .is_some_and(|usage| usage.count_estimate > 0);
        uses_module_ref |= core
            .symbol_uses
            .get(&core.module_ref)
            .is_some_and(|usage| usage.count_estimate > 0);

        import_record_indices.extend(
            (first_generated_import_record..core.import_records.len())
                .filter(|index| !is_generated_import_record(core, *index))
                .map(|index| u32::try_from(index).expect("import record count fits in u32")),
        );
        import_record_indices.sort_unstable();
        import_record_indices.dedup();

        if statements.is_empty() {
            continue;
        }

        let can_be_removed_if_unused = {
            let helpers = make_helper_context(|reference| {
                core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                    == SymbolKind::Unbound
            });
            let flags = if core.options.mode == crate::internal::config::Mode::PassThrough {
                StmtsCanBeRemovedIfUnusedFlags::KEEP_EXPORT_CLAUSES
            } else {
                StmtsCanBeRemovedIfUnusedFlags::NONE
            };
            helpers.stmts_can_be_removed_if_unused(&statements, flags)
        };
        let part = Part {
            statements,
            scopes: std::mem::take(&mut core.scopes_for_current_part),
            import_record_indices,
            declared_symbols: std::mem::take(&mut core.declared_symbols),
            symbol_uses: std::mem::take(&mut core.symbol_uses),
            import_symbol_property_uses: std::mem::take(&mut core.import_symbol_property_uses),
            can_be_removed_if_unused,
            ..Part::default()
        };
        if move_before {
            before_parts.push(part);
        } else if move_after {
            after_parts.push(part);
        } else {
            parts.push(part);
        }
    }

    // TypeScript import trimming needs use counts from the whole file. Import
    // parts are hoisted ahead of the other parts, but scanning them must happen
    // after all other parts have been visited so those counts are complete.
    for part in &mut before_parts {
        if part
            .statements
            .iter()
            .any(|statement| matches!(statement.data.as_deref(), Some(StmtData::Import(_))))
        {
            core.declared_symbols = std::mem::take(&mut part.declared_symbols);
            scan_module_metadata_into(core, &mut part.statements, &mut metadata);
            part.declared_symbols = std::mem::take(&mut core.declared_symbols);
        }
    }

    let generated_named_imports = std::mem::take(&mut core.generated_named_imports);
    for (&reference, import) in &generated_named_imports {
        if let Some(part) = before_parts.iter_mut().find(|part| {
            part.import_record_indices
                .contains(&import.import_record_index)
        }) {
            if !part
                .declared_symbols
                .iter()
                .any(|declared| declared.reference == reference)
            {
                part.declared_symbols.push(DeclaredSymbol {
                    reference,
                    is_top_level: true,
                });
            }
        }
    }
    metadata.named_imports.extend(generated_named_imports);

    let mut ordered_parts = vec![Part {
        symbol_uses: HashMap::new(),
        can_be_removed_if_unused: true,
        ..Part::default()
    }];
    ordered_parts.extend(before_parts);
    ordered_parts.extend(parts);
    ordered_parts.extend(after_parts);

    let contains_direct_eval = core.module_scope.as_ref().is_some_and(|scope| {
        scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_direct_eval
    });
    if contains_direct_eval {
        for part in &mut ordered_parts {
            if part
                .declared_symbols
                .iter()
                .any(|symbol| symbol.is_top_level)
            {
                part.can_be_removed_if_unused = false;
            }
        }
    }

    (ordered_parts, metadata, uses_exports_ref, uses_module_ref)
}

fn remove_unused_type_script_import_equals(core: &mut ParserCore, parts: &mut Vec<Part>) {
    loop {
        let mut removed_any = false;
        for part in parts.iter_mut() {
            let mut kept = Vec::with_capacity(part.statements.len());
            for statement in std::mem::take(&mut part.statements) {
                let removal = statement.data.as_deref().and_then(|data| match data {
                    StmtData::Local(local) if local.was_ts_import_equals && !local.is_export => {
                        let declaration = local.declarations.first()?;
                        let BindingData::Identifier(binding) =
                            declaration.binding.data.as_deref()?
                        else {
                            return None;
                        };
                        let symbol_index = usize::try_from(binding.reference.inner_index)
                            .expect("symbol index fits usize");
                        if core.symbols[symbol_index].use_count_estimate != 0 {
                            return None;
                        }

                        let mut value = &declaration.value_or_nil;
                        while let Some(ExprData::Dot(dot)) = value.data.as_deref() {
                            value = &dot.target;
                        }
                        let value_ref = match value.data.as_deref()? {
                            ExprData::Identifier(identifier) => identifier.reference,
                            ExprData::ImportIdentifier(identifier) => identifier.reference,
                            _ => return None,
                        };
                        Some((binding.reference, value_ref))
                    }
                    _ => None,
                });

                let Some((binding_ref, value_ref)) = removal else {
                    kept.push(statement);
                    continue;
                };

                let value_index =
                    usize::try_from(value_ref.inner_index).expect("symbol index fits usize");
                core.symbols[value_index].use_count_estimate -= 1;
                if let Some(usage) = part.symbol_uses.get_mut(&value_ref) {
                    usage.count_estimate -= 1;
                    if usage.count_estimate == 0 {
                        part.symbol_uses.remove(&value_ref);
                    }
                }
                part.declared_symbols
                    .retain(|declared| declared.reference != binding_ref);
                removed_any = true;
            }
            part.statements = kept;
        }

        if !removed_any {
            break;
        }
    }

    let mut index = 0usize;
    parts.retain(|part| {
        let keep = index == 0 || !part.statements.is_empty();
        index += 1;
        keep
    });
}

fn prepend_generated_namespace_import_declarations(
    core: &mut ParserCore,
    named_imports: &HashMap<Ref, NamedImport>,
) {
    let mut generated = named_imports
        .iter()
        .filter_map(|(&reference, import)| {
            let symbol = &core.symbols
                [usize::try_from(reference.inner_index).expect("symbol index fits usize")];
            (symbol.import_item_status == crate::internal::ast::ImportItemStatus::Generated)
                .then_some((import.import_record_index, import.alias.clone(), reference))
        })
        .collect::<Vec<_>>();
    generated.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    if generated.is_empty() {
        return;
    }

    let generated_refs = generated
        .iter()
        .map(|(_, _, reference)| *reference)
        .collect::<HashSet<_>>();
    core.declared_symbols
        .retain(|declared| !generated_refs.contains(&declared.reference));
    let mut declarations = generated
        .into_iter()
        .map(|(_, _, reference)| DeclaredSymbol {
            reference,
            is_top_level: true,
        })
        .collect::<Vec<_>>();
    declarations.append(&mut core.declared_symbols);
    core.declared_symbols = declarations;
}

fn keep_name_expression(core: &mut ParserCore, value: Expr, name: &str) -> Expr {
    let loc = value.loc;
    let mut call = core.call_runtime(
        loc,
        "__name",
        vec![
            value,
            Expr::new(
                loc,
                ExprData::String(StringExpr {
                    value: string_to_utf16(name.as_bytes()),
                    ..StringExpr::default()
                }),
            ),
        ],
    );
    if let Some(ExprData::Call(call)) = call.data.as_deref_mut() {
        call.can_be_unwrapped_if_unused = true;
    }
    call
}

fn keep_declaration_name_statement(
    core: &mut ParserCore,
    loc: Loc,
    reference: Ref,
    name: &str,
) -> Stmt {
    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].flags |=
        crate::internal::ast::SymbolFlags::DID_KEEP_NAME;
    Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: core.call_runtime(
                loc,
                "__name",
                vec![
                    Expr::new(
                        loc,
                        ExprData::Identifier(IdentifierExpr {
                            reference,
                            ..IdentifierExpr::default()
                        }),
                    ),
                    Expr::new(
                        loc,
                        ExprData::String(StringExpr {
                            value: string_to_utf16(name.as_bytes()),
                            ..StringExpr::default()
                        }),
                    ),
                ],
            ),
            is_from_class_or_fn_that_can_be_removed_if_unused: true,
        }),
    )
}

fn class_has_static_name(class: &crate::internal::js_ast::Class) -> bool {
    class.properties.iter().any(|property| {
        property
            .flags
            .contains(crate::internal::js_ast::PropertyFlags::IS_STATIC)
            && matches!(
                property.key.data.as_deref(),
                Some(ExprData::String(value)) if utf16_to_string(&value.value) == b"name"
            )
    })
}

fn class_has_keep_name_static_block(class: &crate::internal::js_ast::Class) -> bool {
    class.properties.iter().any(|property| {
        let Some(block) = &property.class_static_block else {
            return false;
        };
        let [statement] = block.block.statements.as_slice() else {
            return false;
        };
        let Some(StmtData::Expr(statement)) = statement.data.as_deref() else {
            return false;
        };
        let Some(ExprData::Call(call)) = statement.value.data.as_deref() else {
            return false;
        };
        statement.is_from_class_or_fn_that_can_be_removed_if_unused
            && matches!(call.args.as_slice(), [first, second]
                if matches!(first.data.as_deref(), Some(ExprData::This))
                    && matches!(second.data.as_deref(), Some(ExprData::String(_))))
    })
}

fn insert_class_name_static_block(
    core: &mut ParserCore,
    class: &mut crate::internal::js_ast::Class,
    name: &str,
) -> bool {
    if class_has_static_name(class) {
        return false;
    }
    let loc = class.body_loc;
    let call = core.call_runtime(
        loc,
        "__name",
        vec![
            Expr::new(loc, ExprData::This),
            Expr::new(
                loc,
                ExprData::String(StringExpr {
                    value: string_to_utf16(name.as_bytes()),
                    ..StringExpr::default()
                }),
            ),
        ],
    );
    class.properties.insert(
        0,
        crate::internal::js_ast::Property {
            class_static_block: Some(Box::new(crate::internal::js_ast::ClassStaticBlock {
                block: crate::internal::js_ast::BlockStmt {
                    statements: vec![Stmt::new(
                        loc,
                        StmtData::Expr(ExprStmt {
                            value: call,
                            is_from_class_or_fn_that_can_be_removed_if_unused: true,
                        }),
                    )],
                    ..crate::internal::js_ast::BlockStmt::default()
                },
                loc,
            })),
            loc,
            kind: crate::internal::js_ast::PropertyKind::ClassStaticBlock,
            ..crate::internal::js_ast::Property::default()
        },
    );
    true
}

fn keep_inferred_declaration_name(core: &mut ParserCore, value: &mut Expr, reference: Ref) {
    if core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
        .flags
        .contains(crate::internal::ast::SymbolFlags::DID_KEEP_NAME)
    {
        return;
    }
    let name = core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
        .original_name
        .clone();
    if let Some(ExprData::Class(class)) = value.data.as_deref_mut() {
        let has_synthetic_inner_name = class.class.name.is_some_and(|inner| {
            core.symbols[usize::try_from(inner.reference.inner_index).expect("symbol index")]
                .original_name
                == format!("_{name}")
        });
        if (class.class.name.is_none() || has_synthetic_inner_name)
            && !class_has_keep_name_static_block(&class.class)
        {
            insert_class_name_static_block(core, &mut class.class, &name);
        }
        return;
    }
    let can_keep_name = match value.data.as_deref() {
        Some(ExprData::Function(function)) => function.function.name.is_none(),
        Some(ExprData::Arrow(_)) => true,
        _ => false,
    };
    if can_keep_name {
        *value = keep_name_expression(core, std::mem::take(value), &name);
    }
}

type NameToKeep = (Ref, String, bool);

fn existing_name_to_keep(core: &ParserCore, name: Option<LocRef>) -> Option<NameToKeep> {
    name.map(|name| {
        let original_name = core.symbols
            [usize::try_from(name.reference.inner_index).expect("symbol index")]
        .original_name
        .clone();
        (name.reference, original_name, false)
    })
}

fn visit_keep_name_class_blocks(core: &mut ParserCore, class: &mut crate::internal::js_ast::Class) {
    for property in &mut class.properties {
        if let Some(block) = &mut property.class_static_block {
            apply_keep_names_to_statements(core, &mut block.block.statements);
        }
    }
}

fn default_export_name_to_keep(
    core: &mut ParserCore,
    export: &mut crate::internal::js_ast::ExportDefaultStmt,
) -> Option<NameToKeep> {
    match export.value.data.as_deref_mut() {
        Some(StmtData::Function(function)) => {
            apply_keep_names_to_statements(core, &mut function.function.body.block.statements);
            existing_name_to_keep(core, function.function.name).or_else(|| {
                function.function.name = Some(export.default_name);
                Some((export.default_name.reference, "default".into(), true))
            })
        }
        Some(StmtData::Class(class)) => {
            visit_keep_name_class_blocks(core, &mut class.class);
            let name = existing_name_to_keep(core, class.class.name)
                .map_or_else(|| "default".into(), |(_, name, _)| name);
            insert_class_name_static_block(core, &mut class.class, &name);
            None
        }
        _ => None,
    }
}

fn apply_keep_names_to_statements(core: &mut ParserCore, statements: &mut Vec<Stmt>) {
    let mut result = Vec::with_capacity(statements.len());
    for mut statement in std::mem::take(statements) {
        let mut name_to_keep: Option<NameToKeep> = None;
        if let Some(data) = statement.data.as_deref_mut() {
            match data {
                StmtData::Block(block) => {
                    apply_keep_names_to_statements(core, &mut block.statements);
                }
                StmtData::Function(function) => {
                    apply_keep_names_to_statements(
                        core,
                        &mut function.function.body.block.statements,
                    );
                    name_to_keep = existing_name_to_keep(core, function.function.name);
                }
                StmtData::Class(class) => {
                    visit_keep_name_class_blocks(core, &mut class.class);
                    if let Some((_, name, _)) = existing_name_to_keep(core, class.class.name) {
                        insert_class_name_static_block(core, &mut class.class, &name);
                    }
                }
                StmtData::Local(local) => {
                    for declaration in &mut local.declarations {
                        let Some(BindingData::Identifier(binding)) =
                            declaration.binding.data.as_deref()
                        else {
                            continue;
                        };
                        keep_inferred_declaration_name(
                            core,
                            &mut declaration.value_or_nil,
                            binding.reference,
                        );
                    }
                }
                StmtData::ExportDefault(export) => {
                    name_to_keep = default_export_name_to_keep(core, export);
                }
                StmtData::Namespace(namespace) => {
                    apply_keep_names_to_statements(core, &mut namespace.statements);
                }
                StmtData::Try(try_statement) => {
                    apply_keep_names_to_statements(core, &mut try_statement.block.statements);
                    if let Some(catch) = &mut try_statement.catch {
                        apply_keep_names_to_statements(core, &mut catch.block.statements);
                    }
                    if let Some(finally) = &mut try_statement.finally {
                        apply_keep_names_to_statements(core, &mut finally.block.statements);
                    }
                }
                StmtData::Switch(switch) => {
                    for case in &mut switch.cases {
                        apply_keep_names_to_statements(core, &mut case.body);
                    }
                }
                _ => {}
            }
        }
        let loc = statement.loc;
        result.push(statement);
        if let Some((reference, name, rename_default)) = name_to_keep {
            result.push(keep_declaration_name_statement(core, loc, reference, &name));
            if rename_default {
                core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                    .original_name = format!("{}_default", core.source.identifier_name);
            }
        }
    }
    *statements = if core.options.minify_syntax {
        merge_adjacent_expression_statements(result)
    } else {
        result
    };
}

fn top_level_import_record_indices(statement: &Stmt) -> Vec<u32> {
    match statement.data.as_deref() {
        Some(StmtData::Import(import)) => vec![import.import_record_index],
        Some(StmtData::ExportFrom(export)) => vec![export.import_record_index],
        Some(StmtData::ExportStar(export)) => vec![export.import_record_index],
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_lines)]
fn scan_module_metadata(core: &mut ParserCore, statements: &mut [Stmt]) -> ModuleMetadata {
    let mut metadata = ModuleMetadata::default();
    scan_module_metadata_into(core, statements, &mut metadata);
    metadata
        .named_imports
        .extend(std::mem::take(&mut core.generated_named_imports));
    metadata
}

fn trim_unused_imports(core: &ParserCore, import: &mut ImportStmt) -> bool {
    if core.options.mode == crate::internal::config::Mode::Bundle && !core.options.ts.parse {
        return false;
    }
    let unused_import_flags = core.options.ts.config.unused_import_flags();
    let keep_values =
        unused_import_flags.contains(crate::internal::config::TsUnusedImportFlags::KEEP_VALUES);
    let keep_unused_imports = core.options.ts.parse
        && keep_values
        && core.options.mode != crate::internal::config::Mode::Bundle
        && !core.options.minify_identifiers;
    if (!core.options.minify_syntax && !core.options.ts.parse) || keep_unused_imports {
        return false;
    }
    let contains_direct_eval = core.module_scope.as_ref().is_some_and(|scope| {
        scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_direct_eval
    });
    let can_remove_value = |reference: Ref| {
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
            .use_count_estimate
            == 0
            && (core.options.ts.parse || !contains_direct_eval)
    };
    let is_used_in_typescript = |reference: Ref| {
        keep_values
            || core
                .ts_use_counts
                .get(usize::try_from(reference.inner_index).expect("symbol index"))
                .copied()
                .unwrap_or_default()
                != 0
    };

    let mut found_imports = false;
    let mut is_unused_in_typescript = true;
    if let Some(default_name) = import.default_name {
        found_imports = true;
        if core.options.ts.parse && is_used_in_typescript(default_name.reference) {
            is_unused_in_typescript = false;
        }
        if can_remove_value(default_name.reference) {
            import.default_name = None;
        }
    }
    if import.star_name_loc.is_some() {
        found_imports = true;
        if core.options.ts.parse && is_used_in_typescript(import.namespace_ref) {
            is_unused_in_typescript = false;
        }
        if can_remove_value(import.namespace_ref) {
            import.star_name_loc = None;
        }
    }
    if let Some(items) = &mut import.items {
        found_imports = true;
        items.retain(|item| {
            if core.options.ts.parse && is_used_in_typescript(item.name.reference) {
                is_unused_in_typescript = false;
            }
            !can_remove_value(item.name.reference)
        });
        if items.is_empty() {
            import.items = None;
        }
    }

    core.options.ts.parse
        && found_imports
        && is_unused_in_typescript
        && !unused_import_flags.contains(crate::internal::config::TsUnusedImportFlags::KEEP_STMT)
}

#[allow(clippy::too_many_lines)]
fn scan_module_metadata_into(
    core: &mut ParserCore,
    statements: &mut [Stmt],
    metadata: &mut ModuleMetadata,
) {
    for statement in statements {
        let unused_import_record = match statement.data.as_deref_mut() {
            Some(StmtData::Import(import)) => {
                trim_unused_imports(core, import).then_some(import.import_record_index)
            }
            _ => None,
        };
        if let Some(import_record_index) = unused_import_record {
            core.import_records
                [usize::try_from(import_record_index).expect("import record index")]
            .flags |= ImportRecordFlags::IS_UNUSED;
            statement.data = None;
            continue;
        }
        match statement.data.as_deref_mut() {
            Some(StmtData::Import(import)) => {
                core.record_declared_symbol(import.namespace_ref);
                let record = &mut core.import_records
                    [usize::try_from(import.import_record_index).expect("import record index")];
                if import.star_name_loc.is_some() {
                    record.flags |= ImportRecordFlags::CONTAINS_IMPORT_STAR;
                }
                if import.default_name.is_some() {
                    record.flags |= ImportRecordFlags::CONTAINS_DEFAULT_ALIAS;
                }
                if core.options.mode != crate::internal::config::Mode::PassThrough {
                    if let Some(default_name) = import.default_name {
                        metadata.named_imports.insert(
                            default_name.reference,
                            NamedImport {
                                alias: "default".into(),
                                alias_loc: default_name.loc,
                                namespace_ref: import.namespace_ref,
                                import_record_index: import.import_record_index,
                                ..NamedImport::default()
                            },
                        );
                    }
                    if let Some(star_loc) = import.star_name_loc {
                        metadata.named_imports.insert(
                            import.namespace_ref,
                            NamedImport {
                                alias_loc: star_loc,
                                namespace_ref: crate::internal::ast::INVALID_REF,
                                import_record_index: import.import_record_index,
                                alias_is_star: true,
                                ..NamedImport::default()
                            },
                        );
                    }
                    if let Some(items) = &import.items {
                        for item in items {
                            metadata.named_imports.insert(
                                item.name.reference,
                                NamedImport {
                                    alias: item.alias.clone(),
                                    alias_loc: item.alias_loc,
                                    namespace_ref: import.namespace_ref,
                                    import_record_index: import.import_record_index,
                                    ..NamedImport::default()
                                },
                            );
                            if item.alias == "default" {
                                record.flags |= ImportRecordFlags::CONTAINS_DEFAULT_ALIAS;
                            } else if item.alias == "__esModule" {
                                record.flags |= ImportRecordFlags::CONTAINS_ES_MODULE_ALIAS;
                            }
                        }
                    }
                }
            }
            Some(StmtData::ExportDefault(export)) => {
                record_export(
                    core,
                    &mut metadata.named_exports,
                    export.default_name.loc,
                    "default",
                    export.default_name.reference,
                );
            }
            Some(StmtData::ExportClause(export)) => {
                let mut valid_items = Vec::with_capacity(export.items.len());
                for mut item in std::mem::take(&mut export.items) {
                    if ParserCore::is_stored_name_ref(item.name.reference) {
                        let name =
                            String::from_utf8_lossy(core.load_name_from_ref(item.name.reference))
                                .into_owned();
                        item.name.reference = core.find_symbol(item.name.loc, &name).reference;
                    }
                    let symbol_index =
                        usize::try_from(item.name.reference.inner_index).expect("symbol index");
                    if core.symbols[symbol_index].kind == SymbolKind::Unbound {
                        if !core.options.ts.parse {
                            core.add_error_range(
                                crate::internal::js_lexer::range_of_identifier(
                                    &core.source,
                                    item.name.loc,
                                ),
                                format!("{:?} is not declared in this file", item.original_name),
                            );
                        }
                        continue;
                    }
                    record_export(
                        core,
                        &mut metadata.named_exports,
                        item.alias_loc,
                        &item.alias,
                        item.name.reference,
                    );
                    valid_items.push(item);
                }
                export.items = valid_items;
            }
            Some(StmtData::ExportStar(export)) => {
                core.record_declared_symbol(export.namespace_ref);
                let record = &mut core.import_records
                    [usize::try_from(export.import_record_index).expect("import record index")];
                if let Some(alias) = &export.alias {
                    record.flags |= ImportRecordFlags::CONTAINS_IMPORT_STAR;
                    metadata.named_imports.insert(
                        export.namespace_ref,
                        NamedImport {
                            alias_loc: alias.loc,
                            namespace_ref: crate::internal::ast::INVALID_REF,
                            import_record_index: export.import_record_index,
                            alias_is_star: true,
                            is_exported: true,
                            ..NamedImport::default()
                        },
                    );
                    record_export(
                        core,
                        &mut metadata.named_exports,
                        alias.loc,
                        &alias.original_name,
                        export.namespace_ref,
                    );
                } else {
                    metadata
                        .export_star_import_records
                        .push(export.import_record_index);
                }
            }
            Some(StmtData::ExportFrom(export)) => {
                core.record_declared_symbol(export.namespace_ref);
                let mut flags = ImportRecordFlags::default();
                for item in &mut export.items {
                    if ParserCore::is_stored_name_ref(item.name.reference) {
                        item.name.reference =
                            core.new_symbol(SymbolKind::Import, item.original_name.clone());
                    }
                    core.record_declared_symbol(item.name.reference);
                    metadata.named_imports.insert(
                        item.name.reference,
                        NamedImport {
                            alias: item.original_name.clone(),
                            alias_loc: item.name.loc,
                            namespace_ref: export.namespace_ref,
                            import_record_index: export.import_record_index,
                            is_exported: true,
                            ..NamedImport::default()
                        },
                    );
                    record_export(
                        core,
                        &mut metadata.named_exports,
                        item.name.loc,
                        &item.alias,
                        item.name.reference,
                    );
                    if item.original_name == "default" {
                        flags |= ImportRecordFlags::CONTAINS_DEFAULT_ALIAS;
                    } else if item.original_name == "__esModule" {
                        flags |= ImportRecordFlags::CONTAINS_ES_MODULE_ALIAS;
                    }
                }
                core.import_records
                    [usize::try_from(export.import_record_index).expect("import record index")]
                .flags |= flags;
            }
            Some(StmtData::Local(local)) if local.is_export => {
                for declaration in &mut local.declarations {
                    for_each_identifier_binding(
                        &mut declaration.binding,
                        &mut |loc, identifier| {
                            let alias =
                                core.symbols[usize::try_from(identifier.reference.inner_index)
                                    .expect("symbol index")]
                                .original_name
                                .clone();
                            record_export(
                                core,
                                &mut metadata.named_exports,
                                loc,
                                &alias,
                                identifier.reference,
                            );
                        },
                    );
                }
            }
            Some(StmtData::Function(function)) if function.is_export => {
                if let Some(name) = function.function.name {
                    let alias = core.symbols
                        [usize::try_from(name.reference.inner_index).expect("symbol index")]
                    .original_name
                    .clone();
                    record_export(
                        core,
                        &mut metadata.named_exports,
                        name.loc,
                        &alias,
                        name.reference,
                    );
                }
            }
            Some(StmtData::Class(class)) if class.is_export => {
                if let Some(name) = class.class.name {
                    let alias = core.symbols
                        [usize::try_from(name.reference.inner_index).expect("symbol index")]
                    .original_name
                    .clone();
                    record_export(
                        core,
                        &mut metadata.named_exports,
                        name.loc,
                        &alias,
                        name.reference,
                    );
                }
            }
            Some(StmtData::Enum(enumeration)) if enumeration.is_export => {
                let alias = core.symbols[usize::try_from(enumeration.name.reference.inner_index)
                    .expect("symbol index")]
                .original_name
                .clone();
                record_type_script_export(
                    core,
                    &mut metadata.named_exports,
                    enumeration.name.loc,
                    &alias,
                    enumeration.name.reference,
                );
            }
            Some(StmtData::Namespace(namespace)) if namespace.is_export => {
                let alias = core.symbols
                    [usize::try_from(namespace.name.reference.inner_index).expect("symbol index")]
                .original_name
                .clone();
                record_type_script_export(
                    core,
                    &mut metadata.named_exports,
                    namespace.name.loc,
                    &alias,
                    namespace.name.reference,
                );
            }
            _ => {}
        }
    }
}

fn record_export(
    core: &mut ParserCore,
    exports: &mut HashMap<String, NamedExport>,
    loc: Loc,
    alias: &str,
    reference: Ref,
) {
    if exports.contains_key(alias) {
        core.add_error_range(
            crate::internal::logger::Range { loc, len: 0 },
            format!("Multiple exports with the same name {alias:?}"),
        );
    } else {
        exports.insert(
            alias.into(),
            NamedExport {
                reference,
                alias_loc: loc,
            },
        );
    }
}

fn record_type_script_export(
    core: &mut ParserCore,
    exports: &mut HashMap<String, NamedExport>,
    loc: Loc,
    alias: &str,
    reference: Ref,
) {
    // TypeScript declarations such as enums and namespaces can legally merge
    // with another declaration. Declaration binding has already diagnosed
    // incompatible collisions, so a pre-existing export here is the one shared
    // export for the merged declaration instead of a duplicate export.
    if exports.contains_key(alias) {
        return;
    }
    record_export(core, exports, loc, alias, reference);
}

fn declare_top_level_symbols(
    core: &mut ParserCore,
    statements: &mut [Stmt],
) -> Vec<DeclaredSymbol> {
    let mut declared = Vec::new();
    for statement in statements {
        match statement.data.as_deref_mut() {
            Some(StmtData::Local(local)) => {
                let kind = match local.kind {
                    LocalKind::Var => SymbolKind::Hoisted,
                    LocalKind::Const => SymbolKind::Const,
                    LocalKind::Let | LocalKind::Using | LocalKind::AwaitUsing => SymbolKind::Other,
                };
                for declaration in &mut local.declarations {
                    for_each_identifier_binding(
                        &mut declaration.binding,
                        &mut |loc, identifier| {
                            if ParserCore::is_stored_name_ref(identifier.reference) {
                                let name = String::from_utf8_lossy(
                                    core.load_name_from_ref(identifier.reference),
                                )
                                .into_owned();
                                identifier.reference = core.declare_symbol(kind, loc, &name);
                            }
                            record_top_level_symbol(&mut declared, identifier.reference);
                        },
                    );
                }
            }
            Some(StmtData::Function(function)) => {
                if let Some(name) = &mut function.function.name {
                    let kind = if function.function.is_async || function.function.is_generator {
                        SymbolKind::GeneratorOrAsyncFunction
                    } else {
                        SymbolKind::HoistedFunction
                    };
                    bind_loc_ref(core, name, kind, &mut declared);
                }
            }
            Some(StmtData::Class(class)) => {
                if let Some(name) = &mut class.class.name {
                    bind_loc_ref(core, name, SymbolKind::Class, &mut declared);
                }
            }
            Some(StmtData::Enum(enumeration)) => {
                bind_loc_ref(
                    core,
                    &mut enumeration.name,
                    SymbolKind::TsEnum,
                    &mut declared,
                );
            }
            Some(StmtData::Namespace(namespace)) => {
                bind_loc_ref(
                    core,
                    &mut namespace.name,
                    SymbolKind::TsNamespace,
                    &mut declared,
                );
            }
            Some(StmtData::Import(import)) => {
                if let Some(name) = &mut import.default_name {
                    bind_loc_ref(core, name, SymbolKind::Import, &mut declared);
                    core.is_import_item.insert(name.reference);
                }
                if let Some(star_loc) = import.star_name_loc {
                    let mut name = crate::internal::ast::LocRef {
                        loc: star_loc,
                        reference: import.namespace_ref,
                    };
                    bind_loc_ref(core, &mut name, SymbolKind::Import, &mut declared);
                    import.namespace_ref = name.reference;
                }
                if let Some(items) = &mut import.items {
                    for item in items {
                        bind_loc_ref(core, &mut item.name, SymbolKind::Import, &mut declared);
                        core.is_import_item.insert(item.name.reference);
                    }
                }
                let mut entries = HashMap::new();
                if let Some(name) = import.default_name {
                    entries.insert("default".into(), name);
                }
                if let Some(items) = &import.items {
                    for item in items {
                        entries.insert(
                            item.alias.clone(),
                            crate::internal::ast::LocRef {
                                loc: item.name.loc,
                                reference: item.name.reference,
                            },
                        );
                    }
                }
                core.import_items_for_namespace.insert(
                    import.namespace_ref,
                    super::parser_core::NamespaceImportItems {
                        entries,
                        import_record_index: import.import_record_index,
                    },
                );
            }
            Some(StmtData::ExportDefault(export)) => match export.value.data.as_deref_mut() {
                Some(StmtData::Function(function)) => {
                    if let Some(name) = &mut function.function.name {
                        bind_loc_ref(core, name, SymbolKind::HoistedFunction, &mut declared);
                        export.default_name.reference = name.reference;
                    } else {
                        record_top_level_symbol(&mut declared, export.default_name.reference);
                    }
                }
                Some(StmtData::Class(class)) => {
                    if let Some(name) = &mut class.class.name {
                        bind_loc_ref(core, name, SymbolKind::Class, &mut declared);
                        export.default_name.reference = name.reference;
                    } else {
                        record_top_level_symbol(&mut declared, export.default_name.reference);
                    }
                }
                _ => record_top_level_symbol(&mut declared, export.default_name.reference),
            },
            _ => {}
        }
    }
    declared
}

fn bind_loc_ref(
    core: &mut ParserCore,
    name: &mut crate::internal::ast::LocRef,
    kind: SymbolKind,
    declared: &mut Vec<DeclaredSymbol>,
) {
    if ParserCore::is_stored_name_ref(name.reference) {
        let text = String::from_utf8_lossy(core.load_name_from_ref(name.reference)).into_owned();
        name.reference = core.declare_symbol(kind, name.loc, &text);
    }
    record_top_level_symbol(declared, name.reference);
}

fn record_top_level_symbol(declared: &mut Vec<DeclaredSymbol>, reference: Ref) {
    if !declared.iter().any(|symbol| symbol.reference == reference) {
        declared.push(DeclaredSymbol {
            reference,
            is_top_level: true,
        });
    }
}

fn apply_jsx_pragmas(core: &mut ParserCore, lexer: &Lexer) {
    if !core.options.jsx.parse {
        return;
    }
    let runtime = &lexer.jsx_runtime_pragma_comment;
    if !runtime.text.is_empty() {
        match runtime.text.as_str() {
            "automatic" => core.options.jsx.automatic_runtime = true,
            "classic" => core.options.jsx.automatic_runtime = false,
            _ => core.add_warning_range(
                runtime.range,
                format!("Invalid JSX runtime: {:?}", runtime.text),
            ),
        }
    }

    let factory = &lexer.jsx_factory_pragma_comment;
    if !factory.text.is_empty() {
        if core.options.jsx.automatic_runtime {
            core.add_warning_range(
                factory.range,
                "The JSX factory cannot be set when using React's \"automatic\" JSX transform",
            );
        } else {
            let (define, _) = super::parse_define_expr(&factory.text);
            if define.parts.is_empty() {
                core.add_warning_range(
                    factory.range,
                    format!("Invalid JSX factory: {}", factory.text),
                );
            } else {
                core.options.jsx.factory = define;
            }
        }
    }

    let fragment = &lexer.jsx_fragment_pragma_comment;
    if !fragment.text.is_empty() {
        if core.options.jsx.automatic_runtime {
            core.add_warning_range(
                fragment.range,
                "The JSX fragment cannot be set when using React's \"automatic\" JSX transform",
            );
        } else {
            let (define, _) = super::parse_define_expr(&fragment.text);
            if define.parts.is_empty() && define.constant.data.is_none() {
                core.add_warning_range(
                    fragment.range,
                    format!("Invalid JSX fragment: {}", fragment.text),
                );
            } else {
                core.options.jsx.fragment = define;
            }
        }
    }

    let import_source = &lexer.jsx_import_source_pragma_comment;
    if !import_source.text.is_empty() {
        if core.options.jsx.automatic_runtime {
            core.options
                .jsx
                .import_source
                .clone_from(&import_source.text);
        } else {
            core.add_warning_range(
                import_source.range,
                "The JSX import source cannot be set without also enabling React's \"automatic\" JSX transform",
            );
        }
    }
}

fn strip_directive_prologue(
    core: &ParserCore,
    statements: &mut Vec<Stmt>,
) -> (Vec<String>, Vec<Loc>) {
    let mut directives = Vec::new();
    let mut legacy_octal_locs = Vec::new();
    if core
        .options
        .ts_always_strict
        .as_deref()
        .is_some_and(|value| value.value)
    {
        directives.push("use strict".to_owned());
    }

    let mut total_count = 0;
    for statement in statements.iter() {
        if matches!(statement.data.as_deref(), Some(StmtData::Comment(_))) {
            total_count += 1;
            continue;
        }
        let Some(StmtData::Expr(expression)) = statement.data.as_deref() else {
            break;
        };
        let Some(ExprData::String(value)) = expression.value.data.as_deref() else {
            break;
        };
        let start = usize::try_from(statement.loc.start).unwrap_or(usize::MAX);
        if !matches!(core.source.contents.get(start), Some(b'\'' | b'"')) {
            break;
        }

        let directive = String::from_utf8_lossy(&utf16_to_string(&value.value)).into_owned();
        if value.legacy_octal_loc.start > 0 {
            legacy_octal_locs.push(value.legacy_octal_loc);
        }
        if !directives.contains(&directive) {
            directives.push(directive);
        }
        total_count += 1;
    }
    if total_count > 0 {
        let comments = statements
            .drain(..total_count)
            .filter(|statement| matches!(statement.data.as_deref(), Some(StmtData::Comment(_))))
            .collect::<Vec<_>>();
        statements.splice(..0, comments);
    }
    (directives, legacy_octal_locs)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::{HelperCall, lazy_export_ast, parse};
    use crate::internal::{
        compat::JsFeature,
        config::Format,
        helpers::string_to_utf16,
        js_ast::{Expr, ExprData, LocalKind, OpCode, StmtData, StringExpr},
        js_parser::Options,
        logger::{DeferLogKind, Loc, Log, Msg, MsgId, MsgKind, Path, Source},
        runtime,
    };

    fn parse_source(text: &str) -> (crate::internal::js_ast::Ast, bool, Log) {
        parse_source_with_options(text, Options::default())
    }

    fn parse_source_with_options(
        text: &str,
        options: Options,
    ) -> (crate::internal::js_ast::Ast, bool, Log) {
        parse_source_at_path_with_options(text, "", options)
    }

    fn parse_source_at_path_with_options(
        text: &str,
        path: &str,
        options: Options,
    ) -> (crate::internal::js_ast::Ast, bool, Log) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text.as_bytes()),
            identifier_name: "entry".to_owned(),
            key_path: Path {
                text: path.to_owned(),
                ..Path::default()
            },
            ..Source::default()
        };
        let (ast, ok) = parse(log.clone(), source, options);
        (ast, ok, log)
    }

    fn unsupported_bigint_options() -> Options {
        Options {
            unsupported_js_features: crate::internal::compat::JsFeature::BIGINT,
            original_target_env: "es2019".into(),
            ..Options::default()
        }
    }

    fn assert_syntax_guard_message(
        message: &Msg,
        text: &str,
        line: usize,
        column: usize,
        length: usize,
    ) {
        assert_eq!(message.kind, MsgKind::Error);
        assert_eq!(message.data.text, text);
        let location = message
            .data
            .location
            .as_ref()
            .expect("syntax guard location");
        assert_eq!(
            (location.line, location.column, location.length),
            (line, column, length)
        );
    }

    #[test]
    fn parses_a_complete_source_file_into_parts() {
        let (ast, ok, log) = parse_source(
            "#!/usr/bin/env node\n\"use strict\";\nlet [a, b = 2] = values;\nif (a) b++;",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.hashbang, "#!/usr/bin/env node");
        assert_eq!(ast.directives, ["use strict"]);
        assert_eq!(ast.approximate_line_count, 4);
        assert_eq!(ast.parts.len(), 2);
        assert!(ast.parts[0].statements.is_empty());
        assert_eq!(ast.parts[1].statements.len(), 2);
        assert_eq!(ast.parts[1].scopes.len(), 1);
        assert!(ast.module_scope.is_some());
        assert_eq!(ast.symbols.len(), 7);
        assert_eq!(
            ast.module_scope
                .as_ref()
                .expect("module scope")
                .lock()
                .expect("module scope lock")
                .strict_mode,
            crate::internal::js_ast::StrictModeKind::ExplicitStrict
        );
        assert!(matches!(
            ast.parts[1].statements[0].data.as_deref(),
            Some(StmtData::Local(_))
        ));
        assert!(matches!(
            ast.parts[1].statements[1].data.as_deref(),
            Some(StmtData::If(_))
        ));
    }

    #[test]
    fn uses_contextual_warning_kinds_for_unsupported_bigints() {
        let (_, ok, log) = parse_source_with_options(
            "try {\n\
             \x20 direct = 0xCAFE_BABEn;\n\
             \x20 function nested() { return 2n }\n\
             } catch {\n\
             \x20 caught = 3n\n\
             } finally {\n\
             \x20 final = 4n\n\
             }",
            unsupported_bigint_options(),
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 4);
        assert!(messages.iter().all(|message| message.id == MsgId::JsBigInt));
        assert_eq!(
            messages
                .iter()
                .map(|message| message.kind)
                .collect::<Vec<_>>(),
            [
                MsgKind::Debug,
                MsgKind::Warning,
                MsgKind::Warning,
                MsgKind::Warning
            ]
        );
        assert_eq!(
            messages
                .iter()
                .map(|message| {
                    message
                        .data
                        .location
                        .as_ref()
                        .expect("BigInt warning location")
                        .length
                })
                .collect::<Vec<_>>(),
            [12, 2, 2, 2]
        );
    }

    #[test]
    fn downgrades_unsupported_bigint_warnings_in_node_modules() {
        let (_, ok, log) = parse_source_at_path_with_options(
            "value = 123n",
            "/project/node_modules/pkg/index.js",
            unsupported_bigint_options(),
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, MsgId::JsBigInt);
        assert_eq!(messages[0].kind, MsgKind::Debug);
        assert_eq!(
            messages[0]
                .data
                .location
                .as_ref()
                .expect("BigInt debug location")
                .length,
            4
        );
    }

    #[test]
    fn allows_top_level_await() {
        let (ast, ok, log) = parse_source("await work();");
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        assert!(matches!(
            statement.value.data.as_deref(),
            Some(ExprData::Await(_))
        ));
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert_eq!(ast.top_level_await_keyword.loc.start, 0);
        assert_eq!(ast.top_level_await_keyword.len, 5);
        assert_eq!(
            ast.live_top_level_await_keyword,
            ast.top_level_await_keyword
        );
    }

    #[test]
    fn import_meta_and_top_level_for_await_mark_esm() {
        let (ast, ok, log) = parse_source("import.meta.url;");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert_eq!(
            ast.module_scope
                .as_ref()
                .expect("module scope")
                .lock()
                .expect("module scope lock")
                .strict_mode,
            crate::internal::js_ast::StrictModeKind::ImplicitStrictEsm
        );

        let (ast, ok, log) = parse_source("for await (const item of items) {}");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert_eq!(ast.top_level_await_keyword.loc.start, 4);
        assert_eq!(ast.top_level_await_keyword.len, 5);

        let (ast, ok, log) = parse_source(
            "async function run() {\
               for await (const item of items) {}\
               await work();\
             }",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::None);
        assert_eq!(ast.top_level_await_keyword.len, 0);
    }

    #[test]
    fn lowers_plain_exponentiation_and_tracks_runtime_import_metadata() {
        let (ast, ok, log) = parse_source_with_options(
            "let right = a ** b ** c;\
             let left = (a ** b) ** c;\
             let operands = (before(), value) ** (-power);\
             let updates = base++ ** --exponent;",
            Options {
                unsupported_js_features: JsFeature::EXPONENT_OPERATOR,
                ..Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 1);
        assert_eq!(
            ast.import_records[0].source_index.get_index(),
            runtime::SOURCE_INDEX
        );
        assert_eq!(ast.named_imports.len(), 1);
        let (pow_ref, pow_import) = ast
            .named_imports
            .iter()
            .next()
            .expect("generated __pow import");
        assert_eq!(pow_import.alias, "__pow");
        assert_eq!(
            ast.symbols[usize::try_from(pow_ref.inner_index).expect("symbol index")]
                .use_count_estimate,
            6
        );
        assert_eq!(ast.parts[1].import_record_indices, [0]);

        let statements = &ast.parts.last().expect("user code part").statements;
        let initializer = |index: usize| {
            let Some(StmtData::Local(local)) = statements[index].data.as_deref() else {
                panic!("expected local declaration");
            };
            &local.declarations[0].value_or_nil
        };

        let Some(ExprData::Call(right)) = initializer(0).data.as_deref() else {
            panic!("expected outer right-associative __pow call");
        };
        assert!(matches!(
            right.args[1].data.as_deref(),
            Some(ExprData::Call(_))
        ));
        assert!(!matches!(
            right.args[0].data.as_deref(),
            Some(ExprData::Call(_))
        ));

        let Some(ExprData::Call(left)) = initializer(1).data.as_deref() else {
            panic!("expected outer left-grouped __pow call");
        };
        assert!(matches!(
            left.args[0].data.as_deref(),
            Some(ExprData::Call(_))
        ));
        assert!(!matches!(
            left.args[1].data.as_deref(),
            Some(ExprData::Call(_))
        ));

        let Some(ExprData::Call(operands)) = initializer(2).data.as_deref() else {
            panic!("expected __pow call for comma and unary operands");
        };
        assert!(matches!(
            operands.args[0].data.as_deref(),
            Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryComma
        ));
        assert!(matches!(
            operands.args[1].data.as_deref(),
            Some(ExprData::Unary(unary)) if unary.op == OpCode::UnaryNegative
        ));

        let Some(ExprData::Call(updates)) = initializer(3).data.as_deref() else {
            panic!("expected __pow call for update operands");
        };
        assert!(matches!(
            updates.args[0].data.as_deref(),
            Some(ExprData::Unary(unary)) if unary.op == OpCode::UnaryPostIncrement
        ));
        assert!(matches!(
            updates.args[1].data.as_deref(),
            Some(ExprData::Unary(unary)) if unary.op == OpCode::UnaryPreDecrement
        ));
    }

    #[test]
    fn guards_unlowered_exponentiation_assignment_at_the_operator() {
        let (_, ok, log) = parse_source_with_options(
            "let lowered = a ** b;\nbase **= /* decoy **= */ power;",
            Options {
                unsupported_js_features: JsFeature::EXPONENT_OPERATOR,
                original_target_env: "\"es2015\"".into(),
                ..Options::default()
            },
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_syntax_guard_message(
            &messages[0],
            "Transforming exponentiation assignment operators to the configured target \
             environment (\"es2015\") is not supported yet",
            2,
            5,
            3,
        );

        let (ast, ok, log) = parse_source("base **= power;");
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Expr(assignment)) = ast.parts.last().expect("user code part").statements
            [0]
        .data
        .as_deref() else {
            panic!("expected exponentiation assignment expression");
        };
        assert!(matches!(
            assignment.value.data.as_deref(),
            Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryPowerAssign
        ));
    }

    #[test]
    fn guards_unlowered_class_const_and_let_syntax() {
        for (source, feature, name, length) in [
            ("class C {}", JsFeature::CLASS, "class syntax", 5),
            ("const x = 1", JsFeature::CONST_AND_LET, "const", 5),
            ("let x = 1", JsFeature::CONST_AND_LET, "let", 3),
        ] {
            let (_, ok, log) = parse_source_with_options(
                source,
                Options {
                    unsupported_js_features: feature,
                    original_target_env: "\"es5\"".into(),
                    ..Options::default()
                },
            );
            assert!(ok);
            let messages = log.done();
            assert_eq!(messages.len(), 1, "{source:?}");
            assert_syntax_guard_message(
                &messages[0],
                &format!(
                    "Transforming {name} to the configured target environment (\"es5\") is not \
                     supported yet"
                ),
                1,
                0,
                length,
            );
        }
    }

    #[test]
    fn guards_parameter_binding_assignment_and_spread_syntax_with_exact_ranges() {
        let source = "function f({a} = x, ...rest) {}\n\
                      const arrow = ([a, ...[b]], ...[c]) => a\n\
                      const defaults = (value = init) => value;\n\
                      call(...x); new C(...y);\n\
                      [a, {b}] = value;\n\
                      try {} catch ({message}) {}\n\
                      const view = <div>{...children}</div>;\n\
                      (value = init);";
        let mut options = Options {
            unsupported_js_features: JsFeature::DEFAULT_ARGUMENT
                | JsFeature::REST_ARGUMENT
                | JsFeature::DESTRUCTURING
                | JsFeature::NESTED_REST_BINDING,
            original_target_env: "\"es5\"".into(),
            ..Options::default()
        };
        options.jsx.parse = true;
        let (_, ok, log) = parse_source_with_options(source, options);
        assert!(ok);
        let messages = log.done();
        let expected = [
            ("destructuring", 1, 11, 1),
            ("default arguments", 1, 15, 1),
            ("rest arguments", 1, 20, 3),
            ("destructuring", 2, 15, 1),
            ("destructuring", 2, 22, 1),
            ("non-identifier array rest patterns", 2, 22, 1),
            ("rest arguments", 2, 28, 3),
            ("destructuring", 2, 31, 1),
            ("default arguments", 3, 24, 1),
            ("rest arguments", 4, 5, 3),
            ("rest arguments", 4, 18, 3),
            ("destructuring", 5, 0, 1),
            ("destructuring", 5, 4, 1),
            ("destructuring", 6, 14, 1),
            ("rest arguments", 7, 19, 3),
        ];
        assert_eq!(messages.len(), expected.len());
        for (message, (name, line, column, length)) in messages.iter().zip(expected) {
            assert_syntax_guard_message(
                message,
                &format!(
                    "Transforming {name} to the configured target environment (\"es5\") is not \
                     supported yet"
                ),
                line,
                column,
                length,
            );
        }
    }

    #[test]
    fn guards_typescript_ambient_and_abstract_parameter_syntax() {
        let source = "declare function f({a}: T = x, ...rest: U[]): void;\n\
                      abstract class C { abstract m([a, ...[b]]: V, q = y, ...z: W[]): void; }\n\
                      declare function g(...items: any[],): void;";
        let mut options = Options {
            unsupported_js_features: JsFeature::DEFAULT_ARGUMENT
                | JsFeature::REST_ARGUMENT
                | JsFeature::DESTRUCTURING
                | JsFeature::NESTED_REST_BINDING,
            original_target_env: "\"es5\"".into(),
            ..Options::default()
        };
        options.ts.parse = true;
        let (_, ok, log) = parse_source_with_options(source, options);
        assert!(ok);
        let messages = log.done();
        let expected = [
            ("destructuring", 1, 19, 1),
            ("default arguments", 1, 26, 1),
            ("rest arguments", 1, 31, 3),
            ("destructuring", 2, 30, 1),
            ("destructuring", 2, 37, 1),
            ("non-identifier array rest patterns", 2, 37, 1),
            ("default arguments", 2, 48, 1),
            ("rest arguments", 2, 53, 3),
            ("rest arguments", 3, 19, 3),
        ];
        assert_eq!(messages.len(), expected.len());
        for (message, (name, line, column, length)) in messages.iter().zip(expected) {
            assert_syntax_guard_message(
                message,
                &format!(
                    "Transforming {name} to the configured target environment (\"es5\") is not \
                     supported yet"
                ),
                line,
                column,
                length,
            );
        }
    }

    #[test]
    fn does_not_apply_nested_rest_binding_guard_to_assignment_patterns() {
        let (_, ok, log) = parse_source_with_options(
            "[...[value]] = input",
            Options {
                unsupported_js_features: JsFeature::NESTED_REST_BINDING,
                ..Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
    }

    #[test]
    fn guards_generator_family_syntax_across_all_function_forms() {
        let unsupported =
            JsFeature::GENERATOR | JsFeature::ASYNC_AWAIT | JsFeature::ASYNC_GENERATOR;
        for (source, name, column, length) in [
            ("function* f() {}", "generator functions", 8, 1),
            ("(function* () {})", "generator functions", 9, 1),
            ("({ *method() {} })", "generator functions", 3, 1),
            ("class C { *method() {} }", "generator functions", 10, 1),
            ("async function f() {}", "async functions", 0, 5),
            ("(async function () {})", "async functions", 1, 5),
            ("const f = async value => value", "async functions", 10, 5),
            ("const f = async () => 0", "async functions", 10, 5),
            ("({ async method() {} })", "async functions", 3, 5),
            ("class C { async method() {} }", "async functions", 10, 5),
            ("async function* f() {}", "async generator functions", 0, 5),
            ("(async function* () {})", "async generator functions", 1, 5),
            (
                "({ async *method() {} })",
                "async generator functions",
                3,
                5,
            ),
            (
                "class C { async *method() {} }",
                "async generator functions",
                10,
                5,
            ),
        ] {
            let (_, ok, log) = parse_source_with_options(
                source,
                Options {
                    unsupported_js_features: unsupported,
                    original_target_env: "\"es5\"".into(),
                    ..Options::default()
                },
            );
            assert!(ok, "{source:?}");
            let messages = log.done();
            assert_eq!(messages.len(), 1, "{source:?}");
            assert_syntax_guard_message(
                &messages[0],
                &format!(
                    "Transforming {name} to the configured target environment (\"es5\") is not \
                     supported yet"
                ),
                1,
                column,
                length,
            );
        }
    }

    #[test]
    fn follows_generator_dependent_async_guard_rules() {
        for feature in [JsFeature::ASYNC_AWAIT, JsFeature::ASYNC_GENERATOR] {
            let (_, ok, log) = parse_source_with_options(
                "async function f() {}; async function* g() {}",
                Options {
                    unsupported_js_features: feature,
                    ..Options::default()
                },
            );
            assert!(ok);
            assert!(log.done().is_empty());
        }

        let (_, ok, log) = parse_source_with_options(
            "async function f() {}; async function* g() {}",
            Options {
                unsupported_js_features: JsFeature::GENERATOR,
                ..Options::default()
            },
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_syntax_guard_message(
            &messages[0],
            "Transforming generator functions to the configured target environment is not \
             supported yet",
            1,
            37,
            1,
        );

        let (_, ok, log) = parse_source_with_options(
            "async function* f() {}",
            Options {
                unsupported_js_features: JsFeature::GENERATOR
                    | JsFeature::ASYNC_AWAIT
                    | JsFeature::ASYNC_GENERATOR,
                original_target_env: "\"es5\"".into(),
                ..Options::default()
            },
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_syntax_guard_message(
            &messages[0],
            "Transforming async generator functions to the configured target environment \
             (\"es5\") is not supported yet",
            1,
            0,
            5,
        );
    }

    #[test]
    fn guards_every_generator_occurrence() {
        let source = "function* a() {}; function* b() {}; ({ *c() {}, *d() {} })";
        let (_, ok, log) = parse_source_with_options(
            source,
            Options {
                unsupported_js_features: JsFeature::GENERATOR,
                original_target_env: "\"es5\"".into(),
                ..Options::default()
            },
        );
        assert!(ok);
        let messages = log.done();
        let columns = source
            .match_indices('*')
            .map(|(column, _)| column)
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), columns.len());
        for (message, column) in messages.iter().zip(columns) {
            assert_syntax_guard_message(
                message,
                "Transforming generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                1,
                column,
                1,
            );
        }
    }

    #[test]
    fn guards_typescript_generator_family_declarations_and_signatures() {
        let unsupported =
            JsFeature::GENERATOR | JsFeature::ASYNC_AWAIT | JsFeature::ASYNC_GENERATOR;
        for (source, name, column, length) in [
            ("declare function* f(): void;", "generator functions", 16, 1),
            (
                "export declare function* f(): void;",
                "generator functions",
                23,
                1,
            ),
            ("declare async function f(): void;", "async functions", 8, 5),
            (
                "export declare async function f(): void;",
                "async functions",
                15,
                5,
            ),
            (
                "declare async function* f(): void;",
                "async generator functions",
                8,
                5,
            ),
            (
                "export declare async function* f(): void;",
                "async generator functions",
                15,
                5,
            ),
            (
                "abstract class C { abstract *f(): void; }",
                "generator functions",
                28,
                1,
            ),
            (
                "abstract class C { abstract async f(): void; }",
                "async functions",
                28,
                5,
            ),
            (
                "abstract class C { abstract async *f(): void; }",
                "async generator functions",
                28,
                5,
            ),
        ] {
            let mut options = Options {
                unsupported_js_features: unsupported,
                original_target_env: "\"es5\"".into(),
                ..Options::default()
            };
            options.ts.parse = true;
            let (_, ok, log) = parse_source_with_options(source, options);
            assert!(ok, "{source:?}");
            let messages = log.done();
            assert_eq!(messages.len(), 1, "{source:?}");
            assert_syntax_guard_message(
                &messages[0],
                &format!(
                    "Transforming {name} to the configured target environment (\"es5\") is not \
                     supported yet"
                ),
                1,
                column,
                length,
            );
        }
    }

    #[test]
    fn guards_typescript_declare_class_const_and_let_syntax() {
        let mut options = Options {
            unsupported_js_features: JsFeature::CLASS | JsFeature::CONST_AND_LET,
            original_target_env: "\"es5\"".into(),
            ..Options::default()
        };
        options.ts.parse = true;
        let source = "declare class C {};\n\
                      declare const x: number;\n\
                      declare let y: number;\n\
                      export declare class D {};\n\
                      export declare const z: number;\n\
                      export declare let w: number;";
        let (_, ok, log) = parse_source_with_options(source, options);
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 6);
        for (message, (name, line, column, length)) in messages.iter().zip([
            ("class syntax", 1, 8, 5),
            ("const", 2, 8, 5),
            ("let", 3, 8, 3),
            ("class syntax", 4, 15, 5),
            ("const", 5, 15, 5),
            ("let", 6, 15, 3),
        ]) {
            assert_syntax_guard_message(
                message,
                &format!(
                    "Transforming {name} to the configured target environment (\"es5\") is not \
                     supported yet"
                ),
                line,
                column,
                length,
            );
        }
    }

    #[test]
    fn guards_every_top_level_await_and_allows_nested_async_await() {
        let source = "await a(); await b(); for await (const x of y) {}";
        let (ast, ok, log) = parse_source_with_options(
            source,
            Options {
                unsupported_js_features: JsFeature::TOP_LEVEL_AWAIT,
                original_target_env: "\"es2021\"".into(),
                ..Options::default()
            },
        );
        assert!(ok);
        assert_eq!(ast.top_level_await_keyword.loc.start, 0);
        assert_eq!(ast.top_level_await_keyword.len, 5);
        let messages = log.done();
        assert_eq!(messages.len(), 3);
        for (message, column) in messages.iter().zip([0, 11, 26]) {
            assert_syntax_guard_message(
                message,
                "Top-level await is not available in the configured target environment \
                 (\"es2021\")",
                1,
                column,
                5,
            );
        }

        let (ast, ok, log) = parse_source_with_options(
            "async function f() { await a(); for await (const x of y) {} }",
            Options {
                unsupported_js_features: JsFeature::TOP_LEVEL_AWAIT,
                original_target_env: "\"es2021\"".into(),
                ..Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.top_level_await_keyword.len, 0);
    }

    #[test]
    fn guards_top_level_await_in_non_esm_output_formats() {
        for (format, name) in [(Format::CommonJs, "cjs"), (Format::Iife, "iife")] {
            let (_, ok, log) = parse_source_with_options(
                "await work()",
                Options {
                    output_format: format,
                    ..Options::default()
                },
            );
            assert!(ok);
            let messages = log.done();
            assert_eq!(messages.len(), 1);
            assert_syntax_guard_message(
                &messages[0],
                &format!(
                    "Top-level await is currently not supported with the {name:?} output format"
                ),
                1,
                0,
                5,
            );
        }
    }

    #[test]
    fn reports_lexer_panics_as_parse_failure() {
        let (ast, ok, log) = parse_source("let x = ;");
        assert!(!ok);
        assert!(ast.parts.is_empty());
        assert!(!log.done().is_empty());
    }

    #[test]
    fn parses_static_imports_and_exports_with_import_records() {
        let (ast, ok, log) = parse_source(
            "import main, {read as load} from 'pkg';\
             import * as helpers from './helpers.js';\
             import 'side-effect';\
             export {load as read};\
             export {value as renamed} from './value.js';\
             export * as namespace from './all.js';\
             export const answer = 42;\
             export default function() {}",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert_eq!(ast.parts[1].statements.len(), 8);
        assert_eq!(
            ast.module_scope
                .as_ref()
                .expect("module scope")
                .lock()
                .expect("module scope lock")
                .strict_mode,
            crate::internal::js_ast::StrictModeKind::ImplicitStrictEsm
        );
        assert_eq!(ast.import_records.len(), 5);
        assert_eq!(
            ast.import_records
                .iter()
                .map(|record| record.path.text.as_str())
                .collect::<Vec<_>>(),
            [
                "pkg",
                "./helpers.js",
                "side-effect",
                "./value.js",
                "./all.js"
            ]
        );
        assert_eq!(ast.parts[1].import_record_indices, [0, 1, 2, 3, 4]);
        assert!(
            ast.import_records[2]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::WAS_ORIGINALLY_BARE_IMPORT)
        );

        let Some(StmtData::Import(import)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected import statement");
        };
        assert!(import.default_name.is_some());
        let items = import.items.as_ref().expect("expected named imports");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].alias, "read");
        assert_eq!(items[0].original_name, "load");

        assert!(matches!(
            ast.parts[1].statements[4].data.as_deref(),
            Some(StmtData::ExportFrom(_))
        ));
        assert!(matches!(
            ast.parts[1].statements[5].data.as_deref(),
            Some(StmtData::ExportStar(export)) if export.alias.is_some()
        ));
        assert!(matches!(
            ast.parts[1].statements[6].data.as_deref(),
            Some(StmtData::Local(local)) if local.is_export
        ));
        assert!(matches!(
            ast.parts[1].statements[7].data.as_deref(),
            Some(StmtData::ExportDefault(_))
        ));
    }

    #[test]
    fn builds_independent_parts_when_tree_shaking_is_enabled() {
        let (ast, ok, log) = parse_source_with_options(
            "console.log('effect');\
             import 'static';\
             const dead = 1, live = 2;\
             import('dynamic')",
            Options {
                mode: crate::internal::config::Mode::Bundle,
                tree_shaking: true,
                ..Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts.len(), 6);
        assert_eq!(ast.import_records.len(), 2);
        assert_eq!(ast.parts[1].import_record_indices, [0]);
        assert!(ast.parts[2].import_record_indices.is_empty());
        assert!(ast.parts[3].import_record_indices.is_empty());
        assert!(ast.parts[4].import_record_indices.is_empty());
        assert_eq!(ast.parts[5].import_record_indices, [1]);
        assert!(ast.parts[1].can_be_removed_if_unused);
        assert!(!ast.parts[2].can_be_removed_if_unused);
        assert!(ast.parts[3].can_be_removed_if_unused);
        assert!(ast.parts[4].can_be_removed_if_unused);
        assert!(!ast.parts[5].can_be_removed_if_unused);

        for part_index in [3_u32, 4] {
            let declared = ast.parts[part_index as usize]
                .declared_symbols
                .iter()
                .find(|symbol| symbol.is_top_level)
                .expect("top-level declaration");
            assert_eq!(
                ast.top_level_symbol_to_parts_from_parser
                    .get(&declared.reference)
                    .expect("symbol-to-part mapping"),
                &[part_index]
            );
        }
    }

    #[test]
    fn validates_local_exports_and_records_reexport_symbols() {
        let (ast, ok, log) = parse_source("const present = 1; export {present, missing};");
        assert!(ok);
        assert_eq!(log.done().len(), 1);
        let Some(StmtData::ExportClause(export)) = ast.parts[1].statements[1].data.as_deref()
        else {
            panic!("expected export clause");
        };
        assert_eq!(export.items.len(), 1);
        assert_eq!(export.items[0].alias, "present");
        assert!(!ast.named_exports.contains_key("missing"));

        let (ast, ok, log) = parse_source(
            "import {x} from 'pkg';\
             export {y as z} from 'other';\
             export * from 'star';",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let declared = ast.parts[1]
            .declared_symbols
            .iter()
            .map(|symbol| symbol.reference)
            .collect::<std::collections::HashSet<_>>();
        let Some(StmtData::Import(import)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected import");
        };
        assert!(declared.contains(&import.namespace_ref));
        assert!(
            declared.contains(
                &import.items.as_ref().expect("import items")[0]
                    .name
                    .reference
            )
        );
        let Some(StmtData::ExportFrom(export)) = ast.parts[1].statements[1].data.as_deref() else {
            panic!("expected re-export");
        };
        assert!(declared.contains(&export.namespace_ref));
        assert!(declared.contains(&export.items[0].name.reference));
        let Some(StmtData::ExportStar(export)) = ast.parts[1].statements[2].data.as_deref() else {
            panic!("expected export star");
        };
        assert!(declared.contains(&export.namespace_ref));
        assert_eq!(declared.len(), 5);
    }

    #[test]
    fn static_module_declarations_require_module_scope() {
        let (ast, ok, log) = parse_source(
            "function nested() {\
               import value from 'package';\
               export const item = 1;\
               import('dynamic');\
               import.meta.url;\
             }",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 2);
        assert_eq!(ast.import_records.len(), 2);
        assert_eq!(
            ast.import_records
                .iter()
                .map(|record| record.path.text.as_str())
                .collect::<Vec<_>>(),
            ["package", "dynamic"]
        );
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
    }

    #[test]
    fn top_level_bindings_are_declared_before_commonjs_symbols() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"var exports; let local; function fn() {} class Item {}"[..]),
            identifier_name: "entry".to_owned(),
            ..Source::default()
        };
        let (ast, ok) = parse(
            log.clone(),
            source,
            Options {
                mode: crate::internal::config::Mode::ConvertFormat,
                ..Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].declared_symbols.len(), 5);
        assert_eq!(
            ast.parts[1]
                .declared_symbols
                .iter()
                .filter(|symbol| symbol.is_top_level)
                .count(),
            4
        );
        let Some(StmtData::Local(exports)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected exports declaration");
        };
        let Some(crate::internal::js_ast::BindingData::Identifier(exports_binding)) =
            exports.declarations[0].binding.data.as_deref()
        else {
            panic!("expected identifier binding");
        };
        assert_eq!(ast.exports_ref, exports_binding.reference);
        assert_eq!(
            ast.symbols[usize::try_from(ast.exports_ref.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::Hoisted
        );
    }

    #[test]
    fn resolves_top_level_identifier_uses_and_records_counts() {
        let (ast, ok, log) = parse_source("let value = external; value + external;");
        assert!(ok);
        assert!(log.done().is_empty());
        let value_ref = ast
            .symbols
            .iter()
            .position(|symbol| symbol.original_name == "value")
            .map(|index| crate::internal::ast::Ref {
                source_index: 0,
                inner_index: u32::try_from(index).expect("symbol index"),
            })
            .expect("value symbol");
        let external_ref = ast
            .symbols
            .iter()
            .position(|symbol| symbol.original_name == "external")
            .map(|index| crate::internal::ast::Ref {
                source_index: 0,
                inner_index: u32::try_from(index).expect("symbol index"),
            })
            .expect("external symbol");
        assert_eq!(ast.parts[1].symbol_uses[&value_ref].count_estimate, 1);
        assert_eq!(ast.parts[1].symbol_uses[&external_ref].count_estimate, 2);
        assert_eq!(
            ast.symbols[usize::try_from(external_ref.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::Unbound
        );
    }

    #[test]
    fn records_nested_function_scope_order_and_parentage() {
        let (ast, ok, log) =
            parse_source("function outer(a) { return function inner(b) { return a + b } }");
        assert!(ok);
        assert!(log.done().is_empty());
        let kinds = ast.parts[1]
            .scopes
            .iter()
            .map(|scope| scope.lock().expect("scope lock").kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                crate::internal::js_ast::ScopeKind::Entry,
                crate::internal::js_ast::ScopeKind::FunctionArgs,
                crate::internal::js_ast::ScopeKind::FunctionBody,
                crate::internal::js_ast::ScopeKind::FunctionArgs,
                crate::internal::js_ast::ScopeKind::FunctionBody,
            ]
        );
        for index in 1..ast.parts[1].scopes.len() {
            let parent = ast.parts[1].scopes[index]
                .lock()
                .expect("scope lock")
                .parent
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .expect("nested scope parent");
            let expected_parent = match index {
                1 => 0,
                2 | 3 => 1 + usize::from(index == 3),
                4 => 3,
                _ => unreachable!(),
            };
            assert!(std::sync::Arc::ptr_eq(
                &parent,
                &ast.parts[1].scopes[expected_parent]
            ));
        }

        let Some(StmtData::Function(outer)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected outer function");
        };
        let Some(crate::internal::js_ast::BindingData::Identifier(argument)) =
            outer.function.args[0].binding.data.as_deref()
        else {
            panic!("expected identifier argument");
        };
        let args_scope = ast.parts[1].scopes[1].lock().expect("args scope lock");
        assert_eq!(args_scope.members["a"].reference, argument.reference);
        assert_eq!(
            args_scope.members["arguments"].reference,
            outer.function.arguments_ref
        );
        drop(args_scope);
        let body_scope = ast.parts[1].scopes[2].lock().expect("body scope lock");
        assert_eq!(body_scope.members["a"].reference, argument.reference);
        assert_eq!(
            body_scope.members["arguments"].reference,
            outer.function.arguments_ref
        );
    }

    #[test]
    fn block_scopes_hold_distinct_lexical_bindings() {
        let (ast, ok, log) =
            parse_source("let outer = 0; { let outer = 1; const inner = outer; } outer;");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].scopes.len(), 2);
        let entry = ast.parts[1].scopes[0].lock().expect("entry scope");
        let block = ast.parts[1].scopes[1].lock().expect("block scope");
        let entry_outer = entry.members["outer"].reference;
        let block_outer = block.members["outer"].reference;
        assert_ne!(entry_outer, block_outer);
        assert_eq!(
            ast.symbols[usize::try_from(entry_outer.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::Other
        );
        assert_eq!(
            ast.symbols[usize::try_from(block.members["inner"].reference.inner_index)
                .expect("symbol index")]
            .kind,
            crate::internal::ast::SymbolKind::Const
        );
        assert!(std::sync::Arc::ptr_eq(
            &block
                .parent
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .expect("block parent"),
            &ast.parts[1].scopes[0]
        ));
    }

    #[test]
    fn resolves_nested_function_and_block_identifier_uses() {
        let (ast, ok, log) =
            parse_source("function outer(a) { let b = a; { let a = b; use(a, b); } return a; }");
        assert!(ok);
        assert!(log.done().is_empty());
        let argument_a = ast.parts[1].scopes[1].lock().expect("args scope").members["a"].reference;
        let body_b = ast.parts[1].scopes[2].lock().expect("body scope").members["b"].reference;
        let block_a = ast.parts[1].scopes[3].lock().expect("block scope").members["a"].reference;
        assert_ne!(argument_a, block_a);
        assert_eq!(ast.parts[1].symbol_uses[&argument_a].count_estimate, 2);
        assert_eq!(ast.parts[1].symbol_uses[&body_b].count_estimate, 2);
        assert_eq!(ast.parts[1].symbol_uses[&block_a].count_estimate, 1);
        let use_ref = ast
            .symbols
            .iter()
            .position(|symbol| symbol.original_name == "use")
            .map(|index| crate::internal::ast::Ref {
                source_index: 0,
                inner_index: u32::try_from(index).expect("symbol index"),
            })
            .expect("unbound use symbol");
        assert_eq!(ast.parts[1].symbol_uses[&use_ref].count_estimate, 1);
    }

    #[test]
    fn named_function_expressions_have_a_private_self_binding() {
        let (ast, ok, log) =
            parse_source("const fn = function self(value = self) { return self(value); }; self;");
        assert!(ok);
        assert!(log.done().is_empty());

        let args = ast.parts[1].scopes[1]
            .lock()
            .expect("function arguments scope");
        let self_ref = args.members["self"].reference;
        drop(args);
        let Some(StmtData::Local(local)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected local declaration");
        };
        let Some(ExprData::Function(function)) = local.declarations[0].value_or_nil.data.as_deref()
        else {
            panic!("expected function expression");
        };
        assert_eq!(
            function.function.name.expect("function name").reference,
            self_ref
        );
        assert_eq!(ast.parts[1].symbol_uses[&self_ref].count_estimate, 2);

        let outer_self =
            ast.parts[1].scopes[0].lock().expect("entry scope").members["self"].reference;
        assert_ne!(outer_self, self_ref);
        assert_eq!(ast.parts[1].symbol_uses[&outer_self].count_estimate, 1);
    }

    #[test]
    fn validates_strict_bindings_and_duplicate_parameters() {
        let (_, ok, log) = parse_source("function sloppy(a, a) {}");
        assert!(ok);
        assert!(log.done().is_empty());

        let (_, ok, log) = parse_source(
            "\"use strict\";\
             let eval;\
             let protected;\
             function strict(arguments, duplicate, duplicate) {}",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 4);

        let (_, ok, log) = parse_source(
            "function defaults(a, a = 0) {}\
             function body(eval) { \"use strict\"; with (object) {} }\
             function invalid(a = 0) { \"use strict\"; }\
             ({ method(a, a) {} });\
             class Item { method(a, a) {} }",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 6);
    }

    #[test]
    fn validates_assignment_targets_and_marks_mutated_symbols() {
        let (ast, ok, log) = parse_source(
            "let a, b, c, d;\
             a = 1;\
             b += 2;\
             c++;\
             [a, , b] = items;\
             ({value: c, ...d} = object);",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let entry = ast.parts[1].scopes[0].lock().expect("entry scope");
        for name in ["a", "b", "c", "d"] {
            let reference = entry.members[name].reference;
            assert!(
                ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                    .flags
                    .contains(crate::internal::ast::SymbolFlags::COULD_POTENTIALLY_BE_MUTATED),
                "{name}"
            );
        }
        drop(entry);

        let (_, ok, log) = parse_source("1 = value; ++true;");
        assert!(ok);
        assert_eq!(log.done().len(), 2);

        let (_, ok, log) = parse_source("\"use strict\"; eval = 1; arguments++; protected;");
        assert!(ok);
        assert_eq!(log.done().len(), 3);
    }

    #[test]
    fn reports_writes_to_constants_and_imports() {
        let (_, ok, log) = parse_source("const value = 1; value = 2;");
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Warning);

        let (_, ok, log) = parse_source("import {value} from 'package'; value = 2;");
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Error);

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"const value = 1; value = 2;"[..]),
            identifier_name: "entry".to_owned(),
            ..Source::default()
        };
        let (_, ok) = parse(
            log.clone(),
            source,
            Options {
                mode: crate::internal::config::Mode::Bundle,
                ..Options::default()
            },
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Error);
    }

    #[test]
    fn rejects_bare_delete_in_strict_scopes() {
        let (_, ok, log) = parse_source(
            "delete sloppy;\
             function nested() { \"use strict\"; delete value; delete object.value; }\
             class Item { method() { delete classValue; } }",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 2);
    }

    #[test]
    fn rejects_legacy_octal_syntax_in_strict_and_template_contexts() {
        let (_, ok, log) = parse_source(r#""use strict"; let number = 010; let string = "\1";"#);
        assert!(ok);
        assert_eq!(log.done().len(), 2);

        let (_, ok, log) = parse_source(r#"let number = 010; let string = "\1";"#);
        assert!(ok);
        assert!(log.done().is_empty());

        let (_, ok, log) = parse_source(r"let template = `\1`;");
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (_, ok, log) = parse_source(r#""\1"; "use strict";"#);
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (_, ok, log) = parse_source(r#""\1"; export {};"#);
        assert!(ok);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn validates_new_target_and_class_arguments_contexts() {
        let (_, ok, log) = parse_source("new.target; const arrow = () => new.target;");
        assert!(ok);
        assert_eq!(log.done().len(), 2);

        let (_, ok, log) = parse_source(
            "function outer(value = new.target) {\
               new.target;\
               return () => new.target;\
             }",
        );
        assert!(ok);
        assert!(log.done().is_empty());

        let (_, ok, log) = parse_source(
            "class Item {\
               field = new.target;\
               invalid = arguments;\
               method() { return arguments; }\
               static { new.target; arguments; }\
             }",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 2);
    }

    #[test]
    fn direct_eval_pins_the_containing_scope_chain() {
        let (ast, ok, log) =
            parse_source("let top; function run(param) { let local; eval(code); }");
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Function(function)) = ast.parts[1].statements[1].data.as_deref() else {
            panic!("expected function");
        };
        let Some(StmtData::Expr(expression)) =
            function.function.body.block.statements[1].data.as_deref()
        else {
            panic!("expected call");
        };
        assert!(matches!(
            expression.value.data.as_deref(),
            Some(ExprData::Call(call))
                if call.kind == crate::internal::js_ast::CallKind::DirectEval
        ));
        for scope in &ast.parts[1].scopes {
            let scope = scope.lock().expect("scope lock");
            assert!(scope.contains_direct_eval, "{:?}", scope.kind);
        }
        for name in ["top", "run"] {
            let reference =
                ast.parts[1].scopes[0].lock().expect("entry scope").members[name].reference;
            assert!(
                ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                    .flags
                    .contains(crate::internal::ast::SymbolFlags::MUST_NOT_BE_RENAMED)
            );
        }
        for (scope_index, name) in [(1, "param"), (2, "local")] {
            let reference = ast.parts[1].scopes[scope_index]
                .lock()
                .expect("function scope")
                .members[name]
                .reference;
            assert!(
                ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                    .flags
                    .contains(crate::internal::ast::SymbolFlags::MUST_NOT_BE_RENAMED)
            );
        }

        let (ast, ok, log) = parse_source("(0, eval)(code); eval?.(code);");
        assert!(ok);
        assert!(log.done().is_empty());
        for statement in &ast.parts[1].statements {
            let Some(StmtData::Expr(expression)) = statement.data.as_deref() else {
                panic!("expected expression");
            };
            assert!(matches!(
                expression.value.data.as_deref(),
                Some(ExprData::Call(call))
                    if call.kind == crate::internal::js_ast::CallKind::Normal
            ));
        }
        assert!(
            !ast.parts[1].scopes[0]
                .lock()
                .expect("entry scope")
                .contains_direct_eval
        );
    }

    #[test]
    fn commonjs_wrapper_usage_classifies_the_module() {
        let parse_with_mode = |text: &'static [u8], mode| {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let source = Source {
                contents: Arc::from(text),
                identifier_name: "entry".to_owned(),
                ..Source::default()
            };
            let (ast, ok) = parse(
                log.clone(),
                source,
                Options {
                    mode,
                    ..Options::default()
                },
            );
            (ast, ok, log)
        };

        let (ast, ok, log) = parse_with_mode(
            b"exports.value = 1; module.exports = 2;",
            crate::internal::config::Mode::Bundle,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.exports_kind,
            crate::internal::js_ast::ExportsKind::CommonJs
        );
        assert!(ast.uses_exports_ref);
        assert!(ast.uses_module_ref);

        let (ast, ok, log) = parse_with_mode(
            b"let exports = {}; let module = {}; exports.value = module;",
            crate::internal::config::Mode::ConvertFormat,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::None);
        assert!(!ast.uses_exports_ref);
        assert!(!ast.uses_module_ref);

        let (ast, ok, log) = parse_with_mode(b"eval(code);", crate::internal::config::Mode::Bundle);
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.exports_kind,
            crate::internal::js_ast::ExportsKind::CommonJs
        );
        assert!(ast.uses_exports_ref);
        assert!(ast.uses_module_ref);
    }

    #[test]
    fn arrow_parameters_bind_without_creating_an_arguments_symbol() {
        let (ast, ok, log) =
            parse_source("let outer = 1; const fn = (outer, {x}) => outer + x + arguments;");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].scopes.len(), 3);
        let entry = ast.parts[1].scopes[0].lock().expect("entry scope");
        let args = ast.parts[1].scopes[1].lock().expect("arrow args scope");
        let body = ast.parts[1].scopes[2].lock().expect("arrow body scope");
        let entry_outer = entry.members["outer"].reference;
        let argument_outer = args.members["outer"].reference;
        let argument_x = args.members["x"].reference;
        assert_ne!(entry_outer, argument_outer);
        assert!(!args.members.contains_key("arguments"));
        assert_eq!(body.members["outer"].reference, argument_outer);
        assert_eq!(body.members["x"].reference, argument_x);
        drop(body);
        drop(args);
        drop(entry);
        assert_eq!(ast.parts[1].symbol_uses[&argument_outer].count_estimate, 1);
        assert_eq!(ast.parts[1].symbol_uses[&argument_x].count_estimate, 1);
        let arguments_ref = ast
            .symbols
            .iter()
            .position(|symbol| {
                symbol.original_name == "arguments"
                    && symbol.kind == crate::internal::ast::SymbolKind::Unbound
            })
            .map(|index| crate::internal::ast::Ref {
                source_index: 0,
                inner_index: u32::try_from(index).expect("symbol index"),
            })
            .expect("inherited arguments reference");
        assert_eq!(ast.parts[1].symbol_uses[&arguments_ref].count_estimate, 1);
    }

    #[test]
    fn hoists_var_but_keeps_lexical_bindings_inside_blocks() {
        let (ast, ok, log) =
            parse_source("{ var hoisted = 1; let lexical = 2; } hoisted; lexical;");
        assert!(ok);
        assert!(log.done().is_empty());
        let entry = ast.parts[1].scopes[0].lock().expect("entry scope");
        let block = ast.parts[1].scopes[1].lock().expect("block scope");
        let hoisted = block.members["hoisted"].reference;
        assert_eq!(entry.members["hoisted"].reference, hoisted);
        assert_eq!(
            ast.symbols[usize::try_from(hoisted.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::Hoisted
        );

        let block_lexical = block.members["lexical"].reference;
        let unbound_lexical = entry.members["lexical"].reference;
        assert_ne!(block_lexical, unbound_lexical);
        assert_eq!(
            ast.symbols[usize::try_from(block_lexical.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::Other
        );
        assert_eq!(
            ast.symbols[usize::try_from(unbound_lexical.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::Unbound
        );
        assert_eq!(ast.parts[1].symbol_uses[&hoisted].count_estimate, 1);
        assert_eq!(ast.parts[1].symbol_uses[&unbound_lexical].count_estimate, 1);
    }

    #[test]
    fn builds_linker_facing_named_import_and_export_metadata() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"import d, {x as y} from 'pkg';\
                   import * as ns from 'ns';\
                   export {y as z};\
                   export {default as renamed} from 'other';\
                   export * from 'star';\
                   export * as all from 'all';\
                   export default 1;"[..],
            ),
            identifier_name: "entry".to_owned(),
            ..Source::default()
        };
        let (ast, ok) = parse(
            log.clone(),
            source,
            Options {
                mode: crate::internal::config::Mode::ConvertFormat,
                ..Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.export_keyword.len, 6);
        assert_eq!(ast.named_imports.len(), 5);
        assert_eq!(
            ast.named_exports
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>(),
            ["z", "renamed", "all", "default"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert_eq!(ast.export_star_import_records, [3]);
        assert!(
            ast.import_records[0]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::CONTAINS_DEFAULT_ALIAS)
        );
        assert!(
            ast.import_records[1]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::CONTAINS_IMPORT_STAR)
        );
        assert!(
            ast.import_records[2]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::CONTAINS_DEFAULT_ALIAS)
        );
        assert!(
            ast.import_records[4]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::CONTAINS_IMPORT_STAR)
        );
        assert!(
            ast.named_imports
                .values()
                .any(|import| import.alias == "x" && !import.is_exported)
        );
        assert!(
            ast.named_imports
                .values()
                .any(|import| import.alias == "default" && import.is_exported)
        );
        assert!(
            ast.named_imports
                .values()
                .any(|import| import.alias_is_star && import.is_exported)
        );
    }

    #[test]
    fn converts_constant_dynamic_imports_into_import_records() {
        let (ast, ok, log) = parse_source(
            "const literal = import('one');\
             const runtime = import(name);\
             const source = import.source('asset');",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 2);
        assert_eq!(ast.parts[1].import_record_indices, [0, 1]);
        assert_eq!(ast.import_records[0].path.text, "one");
        assert_eq!(
            ast.import_records[0].kind,
            crate::internal::ast::ImportKind::Dynamic
        );
        assert_eq!(
            ast.import_records[0].phase,
            crate::internal::ast::ImportPhase::Evaluation
        );
        assert_eq!(ast.import_records[1].path.text, "asset");
        assert_eq!(
            ast.import_records[1].phase,
            crate::internal::ast::ImportPhase::Source
        );

        let values = ast.parts[1]
            .statements
            .iter()
            .map(|statement| {
                let Some(StmtData::Local(local)) = statement.data.as_deref() else {
                    panic!("expected local statement");
                };
                local.declarations[0].value_or_nil.data.as_deref()
            })
            .collect::<Vec<_>>();
        assert!(matches!(values[0], Some(ExprData::ImportString(_))));
        assert!(matches!(values[1], Some(ExprData::ImportCall(_))));
        assert!(matches!(values[2], Some(ExprData::ImportString(_))));
    }

    #[test]
    fn converts_constant_require_calls_into_import_records() {
        let (ast, ok, log) = parse_source(
            "const loaded = require('one');\
             const resolved = require.resolve('two');\
             const runtime = require(name);",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 2);
        assert_eq!(ast.parts[1].import_record_indices, [0, 1]);
        assert_eq!(
            ast.import_records
                .iter()
                .map(|record| (record.path.text.as_str(), record.kind))
                .collect::<Vec<_>>(),
            [
                ("one", crate::internal::ast::ImportKind::Require),
                ("two", crate::internal::ast::ImportKind::RequireResolve)
            ]
        );
        let values = ast.parts[1]
            .statements
            .iter()
            .map(|statement| {
                let Some(StmtData::Local(local)) = statement.data.as_deref() else {
                    panic!("expected local statement");
                };
                local.declarations[0].value_or_nil.data.as_deref()
            })
            .collect::<Vec<_>>();
        assert!(matches!(values[0], Some(ExprData::RequireString(_))));
        assert!(matches!(values[1], Some(ExprData::RequireResolveString(_))));
        assert!(matches!(values[2], Some(ExprData::Call(_))));
    }

    #[test]
    fn shadowed_require_calls_are_not_import_records() {
        let (ast, ok, log) = parse_source(
            "function load(require) {\
               return require('local') + require.resolve('also-local');\
             }\
             const global = require('package');",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 1);
        assert_eq!(ast.import_records[0].path.text, "package");

        let Some(StmtData::Function(function)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected function");
        };
        let Some(StmtData::Return(return_statement)) =
            function.function.body.block.statements[0].data.as_deref()
        else {
            panic!("expected return");
        };
        let Some(ExprData::Binary(binary)) = return_statement.value_or_nil.data.as_deref() else {
            panic!("expected binary expression");
        };
        assert!(matches!(
            binary.left.data.as_deref(),
            Some(ExprData::Call(_))
        ));
        assert!(matches!(
            binary.right.data.as_deref(),
            Some(ExprData::Call(_))
        ));
    }

    #[test]
    fn catch_bindings_shadow_outer_names_in_a_dedicated_scope() {
        let (ast, ok, log) = parse_source(
            "let error = outer;\
             try { throw value; } catch (error) { use(error); }\
             error;",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.parts[1]
                .scopes
                .iter()
                .map(|scope| scope.lock().expect("scope lock").kind)
                .collect::<Vec<_>>(),
            [
                crate::internal::js_ast::ScopeKind::Entry,
                crate::internal::js_ast::ScopeKind::Block,
                crate::internal::js_ast::ScopeKind::CatchBinding,
                crate::internal::js_ast::ScopeKind::Block,
            ]
        );
        let outer_error =
            ast.parts[1].scopes[0].lock().expect("entry scope").members["error"].reference;
        let catch_error =
            ast.parts[1].scopes[2].lock().expect("catch scope").members["error"].reference;
        assert_ne!(outer_error, catch_error);
        assert_eq!(ast.parts[1].symbol_uses[&outer_error].count_estimate, 1);
        assert_eq!(ast.parts[1].symbol_uses[&catch_error].count_estimate, 1);
    }

    #[test]
    fn labeled_break_and_continue_bind_to_the_label_symbol() {
        let (ast, ok, log) = parse_source("outer: for (;;) { break outer; continue outer; }");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.parts[1]
                .scopes
                .iter()
                .map(|scope| scope.lock().expect("scope lock").kind)
                .collect::<Vec<_>>(),
            [
                crate::internal::js_ast::ScopeKind::Entry,
                crate::internal::js_ast::ScopeKind::Label,
                crate::internal::js_ast::ScopeKind::Block,
                crate::internal::js_ast::ScopeKind::Block,
            ]
        );
        let Some(StmtData::Label(label)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected label statement");
        };
        let label_ref = label.name.reference;
        let Some(StmtData::For(for_statement)) = label.statement.data.as_deref() else {
            panic!("expected labeled loop");
        };
        let Some(StmtData::Block(body)) = for_statement.body.data.as_deref() else {
            panic!("expected loop block");
        };
        assert!(matches!(
            body.statements[0].data.as_deref(),
            Some(StmtData::Break(statement))
                if statement.label.is_some_and(|label| label.reference == label_ref)
        ));
        assert!(matches!(
            body.statements[1].data.as_deref(),
            Some(StmtData::Continue(statement))
                if statement.label.is_some_and(|label| label.reference == label_ref)
        ));
        assert_eq!(ast.parts[1].symbol_uses[&label_ref].count_estimate, 2);

        let (_, ok, log) = parse_source("block: { continue block; }");
        assert!(ok);
        assert!(!log.done().is_empty());
    }

    #[test]
    fn with_and_switch_statements_replay_their_owned_scopes() {
        let (ast, ok, log) = parse_source("let name; with (object) { name; }");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.parts[1]
                .scopes
                .iter()
                .map(|scope| scope.lock().expect("scope lock").kind)
                .collect::<Vec<_>>(),
            [
                crate::internal::js_ast::ScopeKind::Entry,
                crate::internal::js_ast::ScopeKind::With,
                crate::internal::js_ast::ScopeKind::Block,
            ]
        );
        let name_ref =
            ast.parts[1].scopes[0].lock().expect("entry scope").members["name"].reference;
        assert!(
            ast.symbols[usize::try_from(name_ref.inner_index).expect("symbol index")]
                .flags
                .contains(crate::internal::ast::SymbolFlags::MUST_NOT_BE_RENAMED)
        );
        let Some(StmtData::With(with_statement)) = ast.parts[1].statements[1].data.as_deref()
        else {
            panic!("expected with statement");
        };
        let Some(StmtData::Block(block)) = with_statement.body.data.as_deref() else {
            panic!("expected with block");
        };
        let Some(StmtData::Expr(expression)) = block.statements[0].data.as_deref() else {
            panic!("expected expression");
        };
        assert!(matches!(
            expression.value.data.as_deref(),
            Some(ExprData::Identifier(identifier))
                if identifier.reference == name_ref && identifier.must_keep_due_to_with_stmt
        ));

        let (ast, ok, log) =
            parse_source("switch (key) { case 0: let item = 1; break; default: item; }");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].scopes.len(), 2);
        let item_ref =
            ast.parts[1].scopes[1].lock().expect("switch scope").members["item"].reference;
        assert_eq!(ast.parts[1].symbol_uses[&item_ref].count_estimate, 1);
    }

    #[test]
    fn class_name_and_body_scopes_cover_members_and_static_blocks() {
        let (ast, ok, log) = parse_source(
            "class Item extends Base {\
               field = Item;\
               method(arg) { return Item + arg; }\
               static { Item; }\
             }",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.parts[1]
                .scopes
                .iter()
                .map(|scope| scope.lock().expect("scope lock").kind)
                .collect::<Vec<_>>(),
            [
                crate::internal::js_ast::ScopeKind::Entry,
                crate::internal::js_ast::ScopeKind::ClassName,
                crate::internal::js_ast::ScopeKind::ClassBody,
                crate::internal::js_ast::ScopeKind::FunctionArgs,
                crate::internal::js_ast::ScopeKind::FunctionBody,
                crate::internal::js_ast::ScopeKind::ClassStaticInit,
            ]
        );
        let outer_item =
            ast.parts[1].scopes[0].lock().expect("entry scope").members["Item"].reference;
        let inner_item = ast.parts[1].scopes[1]
            .lock()
            .expect("class name scope")
            .members["Item"]
            .reference;
        assert_ne!(outer_item, inner_item);
        assert_eq!(ast.parts[1].symbol_uses[&inner_item].count_estimate, 3);
        for scope in &ast.parts[1].scopes[1..] {
            assert_eq!(
                scope.lock().expect("scope lock").strict_mode,
                crate::internal::js_ast::StrictModeKind::ImplicitStrictClass
            );
        }
        let Some(StmtData::Class(class)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected class statement");
        };
        assert_eq!(class.class.name.expect("class name").reference, outer_item);
    }

    #[test]
    fn binds_private_class_names_and_merges_accessors() {
        let (ast, ok, log) = parse_source(
            "class Box {\
               #value;\
               get #item() { return this.#value; }\
               set #item(value) { this.#value = value; }\
               has(other) { return #value in other; }\
             }",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let class_body = ast.parts[1].scopes[2].lock().expect("class body scope");
        let value_ref = class_body.members["#value"].reference;
        let item_ref = class_body.members["#item"].reference;
        drop(class_body);
        assert_eq!(
            ast.symbols[usize::try_from(value_ref.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::PrivateField
        );
        assert_eq!(
            ast.symbols[usize::try_from(item_ref.inner_index).expect("symbol index")].kind,
            crate::internal::ast::SymbolKind::PrivateGetSetPair
        );
        assert_eq!(ast.parts[1].symbol_uses[&value_ref].count_estimate, 3);
        assert!(
            ast.parts[1]
                .declared_symbols
                .iter()
                .any(|symbol| symbol.reference == value_ref && !symbol.is_top_level)
        );
        assert!(
            ast.parts[1]
                .declared_symbols
                .iter()
                .any(|symbol| symbol.reference == item_ref && !symbol.is_top_level)
        );

        let (_, ok, log) = parse_source("class Duplicate { #field; #field; }");
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (_, ok, log) = parse_source("class Missing { method() { return this.#field; } }");
        assert!(ok);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn visits_type_script_classes_after_erasing_type_only_syntax() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "class Box<T> extends Base<T> implements Readable<T> {\
               declare hidden: string;\
               readonly value!: T;\
               readonly [key: string]: T;\
               map<U>(input: U): T { return this.value; }\
               static accessor count: number = initialize();\
               accessor #slot: number = 0;\
               constructor(public id: string, readonly size: number = 1, readonly = 2) {}\
             }",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Class(class)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected TypeScript class statement");
        };
        assert_eq!(class.class.properties.len(), 5);
        assert_eq!(
            class.class.properties[1].kind,
            crate::internal::js_ast::PropertyKind::Method
        );
        assert_eq!(
            class.class.properties[2].kind,
            crate::internal::js_ast::PropertyKind::AutoAccessor
        );
        assert!(
            class.class.properties[2]
                .flags
                .contains(crate::internal::js_ast::PropertyFlags::IS_STATIC)
        );
        assert_eq!(
            ast.symbols[usize::try_from(
                ast.parts[1].scopes[2].lock().expect("class body").members["#slot"]
                    .reference
                    .inner_index
            )
            .expect("symbol index")]
            .kind,
            crate::internal::ast::SymbolKind::PrivateGetSetPair
        );
        let Some(ExprData::Function(constructor)) =
            class.class.properties[4].value_or_nil.data.as_deref()
        else {
            panic!("expected constructor");
        };
        assert!(constructor.function.args[0].is_typescript_ctor_field);
        assert!(constructor.function.args[1].is_typescript_ctor_field);
        assert!(!constructor.function.args[2].is_typescript_ctor_field);
    }

    #[test]
    fn erases_type_script_ambient_declarations_and_abstract_modifiers() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "declare class Shape<T> extends Base { value: T; method<U>(input: U): void; }\
             declare function load<T>(input: T): Promise<T>;\
             declare const version: string;\
             declare let state: number;\
             declare enum Ambient { A, B }\
             declare namespace AmbientSpace { export const value: number; }\
             declare module 'package' { export interface API { value: string } }\
             declare global { interface Window { value: string } }\
             module 'other' { export type Value = string; }\
             abstract class Runtime<T> {\
               abstract value: T;\
               method(): T { return this.value; }\
             }\
             export abstract class Public { abstract value: number; method() {} }",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 11);
        assert!(
            ast.parts[1].statements[..9]
                .iter()
                .all(|statement| statement.data.is_none())
        );
        let Some(StmtData::Class(runtime)) = ast.parts[1].statements[9].data.as_deref() else {
            panic!("expected abstract runtime class");
        };
        assert_eq!(runtime.class.properties.len(), 1);
        let Some(StmtData::Class(public)) = ast.parts[1].statements[10].data.as_deref() else {
            panic!("expected exported abstract class");
        };
        assert_eq!(public.class.properties.len(), 1);
        assert!(public.is_export);
        assert!(ast.named_exports.contains_key("Public"));
    }

    #[test]
    fn erases_type_script_overload_signatures() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "function format(value: string): string;\
             function format(value: number): string;\
             function format(value) { return value + ''; }\
             class Service {\
               run(value: string): void;\
               run(value) { consume(value); }\
             }",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert!(ast.parts[1].statements[0].data.is_none());
        assert!(ast.parts[1].statements[1].data.is_none());
        assert!(matches!(
            ast.parts[1].statements[2].data.as_deref(),
            Some(StmtData::Function(function)) if function.function.has_body
        ));
        let Some(StmtData::Class(class)) = ast.parts[1].statements[3].data.as_deref() else {
            panic!("expected class with overloaded method");
        };
        assert_eq!(class.class.properties.len(), 1);
        assert_eq!(
            class.class.properties[0].kind,
            crate::internal::js_ast::PropertyKind::Method
        );
    }

    #[test]
    fn parses_type_script_namespaces_and_nested_exports() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "namespace Tools {\
               export const version = 1;\
               export function run() { return version; }\
               interface Hidden { value: string }\
             }\
             export namespace Public { export class Item {} }\
             namespace Outer.Inner { export const value = 1; }\
             namespace.value;\
             module.exports;",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 8);
        assert!(matches!(
            ast.parts[1].statements[0].data.as_deref(),
            Some(StmtData::Local(local)) if local.kind == LocalKind::Var && !local.is_export
        ));
        assert!(matches!(
            ast.parts[1].statements[2].data.as_deref(),
            Some(StmtData::Local(local)) if local.kind == LocalKind::Var && local.is_export
        ));
        assert!(ast.named_exports.contains_key("Public"));
        assert!(matches!(
            ast.parts[1].statements[4].data.as_deref(),
            Some(StmtData::Local(local)) if local.kind == LocalKind::Var
        ));
        assert!(matches!(
            ast.parts[1].statements[6].data.as_deref(),
            Some(StmtData::Expr(_))
        ));
        assert!(matches!(
            ast.parts[1].statements[7].data.as_deref(),
            Some(StmtData::Expr(_))
        ));
        assert!(
            !ast.parts[1]
                .statements
                .iter()
                .any(|statement| matches!(statement.data.as_deref(), Some(StmtData::Namespace(_))))
        );
    }

    #[test]
    fn parses_type_script_import_and_export_assignments() {
        let mut options = Options::default();
        options.ts.parse = true;
        options.mode = crate::internal::config::Mode::Bundle;
        let (ast, ok, log) = parse_source_with_options(
            "import fs = require('fs');\
             import Path = Library.Path;\
             export = { fs, Path };",
            options.clone(),
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Local(fs)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected import-equals local");
        };
        assert!(fs.was_ts_import_equals);
        assert!(matches!(
            fs.declarations[0].value_or_nil.data.as_deref(),
            Some(ExprData::RequireString(_))
        ));
        let Some(StmtData::Local(path)) = ast.parts[1].statements[1].data.as_deref() else {
            panic!("expected qualified import-equals local");
        };
        assert!(path.was_ts_import_equals);
        assert!(matches!(
            path.declarations[0].value_or_nil.data.as_deref(),
            Some(ExprData::Dot(_))
        ));
        assert!(matches!(
            ast.parts[1].statements[2].data.as_deref(),
            Some(StmtData::ExportEquals(export))
                if matches!(export.value.data.as_deref(), Some(ExprData::Object(_)))
        ));
        assert_eq!(
            ast.exports_kind,
            crate::internal::js_ast::ExportsKind::CommonJs
        );
        assert!(ast.uses_module_ref);

        let (ast, ok, log) =
            parse_source_with_options("export import api = require('./api');", options);
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Local(api)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected exported import-equals local");
        };
        assert!(api.was_ts_import_equals);
        assert!(api.is_export);
        assert!(ast.named_exports.contains_key("api"));
    }

    #[test]
    fn parses_type_script_export_only_syntax() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "export as namespace Toolkit;\
             export default interface Config { value: string }\
             export default abstract class Runtime {\
               abstract value: string;\
               method() {}\
             }",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert!(ast.parts[1].statements[0].data.is_none());
        assert!(ast.parts[1].statements[1].data.is_none());
        assert!(matches!(
            ast.parts[1].statements[2].data.as_deref(),
            Some(StmtData::ExportDefault(export))
                if matches!(
                    export.value.data.as_deref(),
                    Some(StmtData::Class(class)) if class.class.properties.len() == 1
                )
        ));
        assert!(ast.named_exports.contains_key("default"));
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
    }

    #[test]
    fn parses_class_member_and_parameter_decorators() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "@sealed class Example {\
               @field accessor value = 1;\
               @logged method(@inject(() => side()) dependency = () => fallback()) {\
                 return dependency;\
               }\
             }\
             const Decorated = @sealed class {};\
             export @sealed class Public {}",
            options,
        );
        let messages = log.done();
        assert!(
            ok,
            "{:?}",
            messages
                .iter()
                .map(|message| &message.data.text)
                .collect::<Vec<_>>()
        );
        assert!(messages.is_empty());
        let Some(StmtData::Class(example)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected decorated class");
        };
        assert_eq!(example.class.decorators.len(), 1);
        assert_eq!(example.class.properties[0].decorators.len(), 1);
        assert_eq!(example.class.properties[1].decorators.len(), 1);
        let Some(ExprData::Function(method)) =
            example.class.properties[1].value_or_nil.data.as_deref()
        else {
            panic!("expected decorated method");
        };
        assert!(matches!(
            method.function.args[0].decorators[0].value.data.as_deref(),
            Some(ExprData::Call(_))
        ));
        assert!(matches!(
            method.function.args[0].default_or_nil.data.as_deref(),
            Some(ExprData::Arrow(_))
        ));
        assert!(matches!(
            ast.parts[1].statements[1].data.as_deref(),
            Some(StmtData::Local(local))
                if matches!(
                    local.declarations[0].value_or_nil.data.as_deref(),
                    Some(ExprData::Class(class)) if class.class.decorators.len() == 1
                )
        ));
        assert!(matches!(
            ast.parts[1].statements[2].data.as_deref(),
            Some(StmtData::Class(class)) if class.is_export && class.class.decorators.len() == 1
        ));
        assert!(ast.named_exports.contains_key("Public"));
    }

    #[test]
    fn validates_class_constructor_and_prototype_names() {
        let (_, ok, log) = parse_source(
            "class Getter { get constructor() {} }\
             class StaticMethod { static prototype() {} }\
             class Field { constructor; }\
             class StaticField { static prototype; }\
             class Private { #constructor() {} }",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 5);

        let (_, ok, log) = parse_source("class Duplicate { constructor() {} constructor() {} }");
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (_, ok, log) = parse_source(
            "class Valid {\
               static constructor() {}\
               prototype() {}\
             }",
        );
        assert!(ok);
        assert!(log.done().is_empty());
    }

    #[test]
    fn reports_duplicate_object_and_class_properties() {
        let (_, ok, log) = parse_source(
            "const object = {\
               x: 1,\
               x: 2,\
               get y() {},\
               set y(value) {},\
               get y() {}\
             };",
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 2);
        assert!(
            messages
                .iter()
                .all(|message| message.kind == MsgKind::Warning)
        );

        let (_, ok, log) = parse_source("class Item { field; field; static field; }");
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Warning);

        let (_, ok, log) = parse_source("const object = {__proto__: one, __proto__: two};");
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Error);

        let (_, ok, log) = parse_source("({__proto__: one, __proto__: two} = object);");
        assert!(ok);
        assert!(log.done().is_empty());
    }

    #[test]
    fn validates_unlabeled_break_and_continue_contexts() {
        let (_, ok, log) = parse_source("break; continue;");
        assert!(ok);
        assert_eq!(log.done().len(), 2);

        let (_, ok, log) = parse_source(
            "while (condition) {\
               break;\
               continue;\
               function nested() { break; }\
               () => { continue; };\
               class Item { static { break; return; } }\
             }\
             switch (value) { default: break; }",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 4);
    }

    #[test]
    fn validates_declarations_in_single_statement_contexts() {
        let (_, ok, log) = parse_source(
            "if (condition) const one = 1;\
             while (condition) let two;\
             do class Three {} while (condition);\
             for (;;) async function four() {}",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 4);

        let (_, ok, log) =
            parse_source("if (condition) function one() {} label: function two() {}");
        assert!(ok);
        assert!(log.done().is_empty());

        let (_, ok, log) = parse_source(
            "\"use strict\";\
             if (condition) function one() {}\
             label: function two() {}",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 2);

        let (_, ok, log) =
            parse_source("for (;;) function one() {} if (condition) label: function two() {}");
        assert!(ok);
        assert_eq!(log.done().len(), 2);

        let (_, ok, log) = parse_source("export {}; if (condition) function one() {}");
        assert!(ok);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn parses_using_declarations_and_restricts_switch_cases() {
        let (ast, ok, log) = parse_source("using resource = acquire(); use(resource);");
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Local(local)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected using declaration");
        };
        assert_eq!(local.kind, crate::internal::js_ast::LocalKind::Using);
        let Some(crate::internal::js_ast::BindingData::Identifier(binding)) =
            local.declarations[0].binding.data.as_deref()
        else {
            panic!("expected using binding");
        };
        assert_eq!(
            ast.parts[1].symbol_uses[&binding.reference].count_estimate,
            1
        );

        let (_, ok, log) = parse_source("using resource;");
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (_, ok, log) = parse_source(
            "switch (kind) {\
               case 0: using direct = acquire(); break;\
               case 1: { using wrapped = acquire(); }\
             }",
        );
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (ast, ok, log) = parse_source("using\nresource = acquire();");
        assert!(ok);
        assert!(log.done().is_empty());
        assert!(matches!(
            ast.parts[1].statements[0].data.as_deref(),
            Some(StmtData::Expr(_))
        ));
    }

    #[test]
    fn parses_await_using_declarations() {
        let (ast, ok, log) = parse_source("await using resource = acquire(); use(resource);");
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Local(local)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected await using declaration");
        };
        assert_eq!(local.kind, crate::internal::js_ast::LocalKind::AwaitUsing);
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert_eq!(ast.top_level_await_keyword.loc.start, 0);
        assert_eq!(ast.top_level_await_keyword.len, 5);

        let (ast, ok, log) = parse_source(
            "async function load() {\
               await using resource = acquire();\
               use(resource);\
             }",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::None);
        assert_eq!(ast.top_level_await_keyword.len, 0);

        let (_, ok, log) = parse_source("await using resource;");
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (_, ok, log) = parse_source("function load() { await using resource = acquire(); }");
        assert!(!ok);
        assert_eq!(log.done().len(), 1);

        let (_, ok, log) =
            parse_source("switch (kind) { case 0: await using resource = acquire(); }");
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let (ast, ok, log) = parse_source("await using\nresource = acquire();");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 2);
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected await expression statement");
        };
        assert!(matches!(
            statement.value.data.as_deref(),
            Some(ExprData::Await(_))
        ));
    }

    #[test]
    fn parses_using_declarations_in_for_loops() {
        let (ast, ok, log) = parse_source(
            "for (using item of items) use(item);\
             for (await using resource of resources) use(resource);\
             for await (using stream of streams) use(stream);",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 3);
        let Some(StmtData::ForOf(first)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected for-of statement");
        };
        assert!(matches!(
            first.init.data.as_deref(),
            Some(StmtData::Local(local)) if local.kind == crate::internal::js_ast::LocalKind::Using
        ));
        let Some(StmtData::ForOf(second)) = ast.parts[1].statements[1].data.as_deref() else {
            panic!("expected await-using for-of statement");
        };
        assert!(matches!(
            second.init.data.as_deref(),
            Some(StmtData::Local(local))
                if local.kind == crate::internal::js_ast::LocalKind::AwaitUsing
        ));
        let Some(StmtData::ForOf(third)) = ast.parts[1].statements[2].data.as_deref() else {
            panic!("expected for-await-of statement");
        };
        assert!(matches!(
            third.init.data.as_deref(),
            Some(StmtData::Local(local)) if local.kind == crate::internal::js_ast::LocalKind::Using
        ));
        assert_ne!(third.await_range.len, 0);
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);

        for source in [
            "for (using item in items) ;",
            "for (using item;;) ;",
            "for (using item = value of items) ;",
            "for (await using item in items) ;",
            "for (await using item = value;;) ;",
            "for (await using item = value of items) ;",
        ] {
            let (_, ok, log) = parse_source(source);
            assert!(ok, "{source}");
            assert_eq!(log.done().len(), 1, "{source}");
        }
    }

    #[test]
    fn parses_jsx_elements_attributes_children_and_fragments() {
        let mut options = Options::default();
        options.jsx.parse = true;
        options.jsx.preserve = true;
        let (ast, ok, log) = parse_source_with_options(
            "const view = <Panel title=\"Hi\" disabled {...props}>\
               <span>{name}</span>\
               <>{...items}</>\
             </Panel>;",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Local(local)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected local statement");
        };
        let Some(ExprData::JsxElement(element)) =
            local.declarations[0].value_or_nil.data.as_deref()
        else {
            panic!("expected JSX element");
        };
        let Some(ExprData::Identifier(component)) = element.tag_or_nil.data.as_deref() else {
            panic!("expected component identifier");
        };
        assert!(
            ast.symbols[usize::try_from(component.reference.inner_index).expect("symbol index")]
                .flags
                .contains(
                    crate::internal::ast::SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX
                )
        );
        assert_eq!(element.properties.len(), 3);
        assert_eq!(element.nullable_children.len(), 2);
        assert!(matches!(
            element.nullable_children[0].data.as_deref(),
            Some(ExprData::JsxElement(child))
                if matches!(child.tag_or_nil.data.as_deref(), Some(ExprData::String(_)))
        ));
        assert!(matches!(
            element.nullable_children[1].data.as_deref(),
            Some(ExprData::JsxElement(fragment)) if fragment.tag_or_nil.data.is_none()
        ));
        for name in ["Panel", "props", "name", "items"] {
            let usage = ast.parts[1]
                .symbol_uses
                .iter()
                .find(|(reference, _)| {
                    ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                        .original_name
                        == name
                })
                .map(|(_, usage)| usage)
                .expect("JSX reference symbol");
            assert_eq!(usage.count_estimate, 1, "{name}");
        }

        let mut options = Options::default();
        options.jsx.parse = true;
        options.jsx.preserve = true;
        let (ast, ok, log) = parse_source_with_options("<div>Hello &amp; world</div>;", options);
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected JSX expression statement");
        };
        let Some(ExprData::JsxElement(element)) = statement.value.data.as_deref() else {
            panic!("expected JSX element");
        };
        assert!(matches!(
            element.nullable_children[0].data.as_deref(),
            Some(ExprData::JsxText(text)) if text.raw == "Hello &amp; world"
        ));

        let mut options = Options::default();
        options.jsx.parse = true;
        let (_, ok, log) = parse_source_with_options("<One></Two>;", options);
        assert!(ok);
        assert_eq!(log.done().len(), 1);

        let mut options = Options::default();
        options.jsx.parse = true;
        let (_, ok, log) = parse_source_with_options("<div value value />;", options);
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Warning);

        let (_, ok, log) = parse_source("<div />;");
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Error);

        let mut options = Options::default();
        options.jsx.parse = true;
        let (_, ok, log) = parse_source_with_options("<Foo.bad-name />;", options);
        assert!(!ok);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn lowers_classic_jsx_to_factory_calls() {
        let mut options = Options::default();
        options.jsx.parse = true;
        let (ast, ok, log) =
            parse_source_with_options("<div id={value}><Widget /></div>;", options);
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        let Some(ExprData::Call(call)) = statement.value.data.as_deref() else {
            panic!("expected lowered JSX call");
        };
        assert_eq!(call.args.len(), 3);
        assert!(matches!(
            call.target.data.as_deref(),
            Some(ExprData::Dot(dot)) if dot.name == "createElement"
        ));
        assert!(matches!(
            call.args[0].data.as_deref(),
            Some(ExprData::String(_))
        ));
        assert!(matches!(
            call.args[1].data.as_deref(),
            Some(ExprData::Object(object)) if object.properties.len() == 1
        ));
        assert!(matches!(
            call.args[2].data.as_deref(),
            Some(ExprData::Call(child)) if child.args.len() == 2
        ));
        for (name, count) in [("React", 2), ("value", 1), ("Widget", 1)] {
            let usage = ast.parts[1]
                .symbol_uses
                .iter()
                .find(|(reference, _)| {
                    ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                        .original_name
                        == name
                })
                .map(|(_, usage)| usage)
                .expect("lowered JSX reference symbol");
            assert_eq!(usage.count_estimate, count, "{name}");
        }
    }

    #[test]
    fn lowers_automatic_jsx_to_runtime_imports() {
        let mut options = Options::default();
        options.jsx.parse = true;
        options.jsx.automatic_runtime = true;
        let (ast, ok, log) =
            parse_source_with_options("<><div key={id} />{left}{right}</>;", options.clone());
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert_eq!(ast.import_records.len(), 1);
        assert_eq!(ast.import_records[0].path.text, "react/jsx-runtime");
        assert_eq!(ast.named_imports.len(), 3);
        assert_eq!(
            ast.named_imports
                .values()
                .map(|item| item.alias.as_str())
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from(["Fragment", "jsx", "jsxs"])
        );
        let Some(StmtData::Import(_)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected generated runtime import");
        };
        let Some(StmtData::Expr(statement)) = ast.parts[2].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        let Some(ExprData::Call(root)) = statement.value.data.as_deref() else {
            panic!("expected automatic JSX call");
        };
        let Some(ExprData::ImportIdentifier(target)) = root.target.data.as_deref() else {
            panic!("expected runtime import target");
        };
        assert_eq!(ast.named_imports[&target.reference].alias, "jsxs");
        assert_eq!(root.args.len(), 2);
        let Some(ExprData::Object(props)) = root.args[1].data.as_deref() else {
            panic!("expected props object");
        };
        let Some(ExprData::Array(children)) = props.properties[0].value_or_nil.data.as_deref()
        else {
            panic!("expected static children array");
        };
        let Some(ExprData::Call(child)) = children.items[0].data.as_deref() else {
            panic!("expected nested JSX call");
        };
        assert_eq!(child.args.len(), 3);
        let Some(ExprData::ImportIdentifier(target)) = child.target.data.as_deref() else {
            panic!("expected nested runtime import target");
        };
        assert_eq!(ast.named_imports[&target.reference].alias, "jsx");

        let (ast, ok, log) =
            parse_source_with_options("<div {...props} key={id} />;", options.clone());
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 1);
        assert_eq!(ast.import_records[0].path.text, "react");
        let Some(StmtData::Expr(statement)) = ast.parts[2].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        let Some(ExprData::Call(call)) = statement.value.data.as_deref() else {
            panic!("expected createElement fallback");
        };
        let Some(ExprData::ImportIdentifier(target)) = call.target.data.as_deref() else {
            panic!("expected createElement import target");
        };
        assert_eq!(ast.named_imports[&target.reference].alias, "createElement");

        let (_, ok, log) = parse_source_with_options("<div __source=\"plugin\" />;", options);
        assert!(ok);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn isolates_generated_jsx_imports_from_tree_shaken_code_parts() {
        let (ast, ok, log) = parse_source_with_options(
            "const dead = <div />; console.log(<span />);",
            Options {
                jsx: crate::internal::config::JsxOptions {
                    parse: true,
                    automatic_runtime: true,
                    ..crate::internal::config::JsxOptions::default()
                },
                tree_shaking: true,
                ..Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts.len(), 4);
        assert!(matches!(
            ast.parts[1].statements[0].data.as_deref(),
            Some(StmtData::Import(_))
        ));
        assert!(ast.parts[1].import_record_indices == [0]);
        assert!(ast.parts[2].can_be_removed_if_unused);
        assert!(!ast.parts[3].can_be_removed_if_unused);

        let jsx_ref = ast
            .named_imports
            .iter()
            .find(|(_, import)| import.alias == "jsx")
            .map(|(reference, _)| *reference)
            .expect("jsx import");
        assert_eq!(ast.top_level_symbol_to_parts_from_parser[&jsx_ref], [1]);
        assert!(
            ast.parts[2]
                .declared_symbols
                .iter()
                .all(|symbol| symbol.reference != jsx_ref)
        );
        assert_eq!(ast.parts[2].symbol_uses[&jsx_ref].count_estimate, 1);
        assert_eq!(ast.parts[3].symbol_uses[&jsx_ref].count_estimate, 1);
    }

    #[test]
    fn lowers_development_jsx_to_jsx_dev_calls() {
        let mut options = Options::default();
        options.jsx.parse = true;
        options.jsx.automatic_runtime = true;
        options.jsx.development = true;
        let (ast, ok, log) = parse_source_with_options("<><One /><Two /></>;", options.clone());
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 1);
        assert_eq!(ast.import_records[0].path.text, "react/jsx-dev-runtime");
        assert_eq!(
            ast.named_imports
                .values()
                .map(|item| item.alias.as_str())
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from(["Fragment", "jsxDEV"])
        );
        let Some(StmtData::Expr(statement)) = ast.parts[2].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        let Some(ExprData::Call(call)) = statement.value.data.as_deref() else {
            panic!("expected jsxDEV call");
        };
        assert_eq!(call.args.len(), 6);
        assert!(matches!(
            call.args[2].data.as_deref(),
            Some(ExprData::Undefined)
        ));
        assert!(matches!(
            call.args[3].data.as_deref(),
            Some(ExprData::Boolean(true))
        ));
        assert!(matches!(
            call.args[4].data.as_deref(),
            Some(ExprData::Object(source)) if source.properties.len() == 3
        ));
        assert!(matches!(call.args[5].data.as_deref(), Some(ExprData::This)));

        let (_, ok, log) =
            parse_source_with_options("<div __source=\"plugin\" __self={self} />;", options);
        assert!(ok);
        assert_eq!(log.done().len(), 2);
    }

    #[test]
    fn applies_file_level_jsx_pragmas() {
        let mut options = Options::default();
        options.jsx.parse = true;
        let (ast, ok, log) = parse_source_with_options("/** @jsx h */ <div />;", options.clone());
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        let Some(ExprData::Call(call)) = statement.value.data.as_deref() else {
            panic!("expected JSX factory call");
        };
        let Some(ExprData::Identifier(target)) = call.target.data.as_deref() else {
            panic!("expected custom factory identifier");
        };
        assert_eq!(
            ast.symbols[usize::try_from(target.reference.inner_index).expect("symbol index")]
                .original_name,
            "h"
        );

        let (ast, ok, log) =
            parse_source_with_options("/** @jsx h @jsxFrag Fragment */ <></>;", options.clone());
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        let Some(ExprData::Call(call)) = statement.value.data.as_deref() else {
            panic!("expected fragment factory call");
        };
        let Some(ExprData::Identifier(fragment)) = call.args[0].data.as_deref() else {
            panic!("expected custom fragment identifier");
        };
        assert_eq!(
            ast.symbols[usize::try_from(fragment.reference.inner_index).expect("symbol index")]
                .original_name,
            "Fragment"
        );

        let (ast, ok, log) = parse_source_with_options(
            "/** @jsxRuntime automatic @jsxImportSource preact */ <div />;",
            options.clone(),
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records[0].path.text, "preact/jsx-runtime");

        let (_, ok, log) = parse_source_with_options(
            "/** @jsxRuntime automatic @jsx custom */ <div />;",
            options.clone(),
        );
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Warning);

        let (_, ok, log) =
            parse_source_with_options("/** @jsxRuntime invalid */ <div />;", options);
        assert!(ok);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Warning);
    }

    #[test]
    fn parses_type_script_type_only_declarations() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "interface Point extends Base {\
               x: number;\
               nested: { value: string };\
             }\
             type Identifier<T> = T | { id: string };\
             const runtime = 1;",
            options.clone(),
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 3);
        assert!(ast.parts[1].statements[0].data.is_none());
        assert!(ast.parts[1].statements[1].data.is_none());
        assert!(matches!(
            ast.parts[1].statements[2].data.as_deref(),
            Some(StmtData::Local(_))
        ));
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::None);

        let (ast, ok, log) = parse_source_with_options(
            "export interface PublicShape { value: string }\
             export type PublicId = string;",
            options.clone(),
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 2);
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);

        let (ast, ok, log) = parse_source_with_options(
            "type Local = string\n\
             const runtime = 1;\
             export type {PublicShape} from './types';\
             export type * from './more-types';",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 4);
        assert!(matches!(
            ast.parts[1].statements[1].data.as_deref(),
            Some(StmtData::Local(_))
        ));
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert!(ast.import_records.is_empty());
    }

    #[test]
    fn erases_type_script_binding_and_function_annotations() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "const count: number = 1;\
             let value!: string;\
             let pair: [number, string]\n\
             function add<T extends number>(this: Calculator, left: T, right?: number): number {\
               return left + (right || 0);\
             }\
             try { throw 1 } catch (error: unknown) { use(error); }",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 5);
        assert!(matches!(
            ast.parts[1].statements[0].data.as_deref(),
            Some(StmtData::Local(local))
                if local.declarations[0].value_or_nil.data.is_some()
        ));
        assert!(matches!(
            ast.parts[1].statements[1].data.as_deref(),
            Some(StmtData::Local(local))
                if local.declarations[0].value_or_nil.data.is_none()
        ));
        assert!(matches!(
            ast.parts[1].statements[2].data.as_deref(),
            Some(StmtData::Local(_))
        ));
        let Some(StmtData::Function(function)) = ast.parts[1].statements[3].data.as_deref() else {
            panic!("expected annotated function declaration");
        };
        assert_eq!(function.function.args.len(), 2);
        assert!(function.function.args[1].default_or_nil.data.is_none());
        assert!(matches!(
            ast.parts[1].statements[4].data.as_deref(),
            Some(StmtData::Try(_))
        ));

        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "const convert = (value: number, radix?: number): string => value + radix;\
             const empty = (): void => {};\
             const defaulted = (value: string = 'x') => value;",
            options,
        );
        let messages = log.done();
        assert!(
            ok,
            "{:?}",
            messages
                .iter()
                .map(|message| &message.data.text)
                .collect::<Vec<_>>()
        );
        assert!(messages.is_empty());
        for (index, argument_count) in [(0, 2), (1, 0), (2, 1)] {
            let Some(StmtData::Local(local)) = ast.parts[1].statements[index].data.as_deref()
            else {
                panic!("expected arrow declaration");
            };
            let Some(ExprData::Arrow(arrow)) = local.declarations[0].value_or_nil.data.as_deref()
            else {
                panic!("expected arrow function");
            };
            assert_eq!(arrow.args.len(), argument_count);
        }
        let Some(StmtData::Local(defaulted)) = ast.parts[1].statements[2].data.as_deref() else {
            panic!("expected defaulted arrow");
        };
        let Some(ExprData::Arrow(defaulted)) =
            defaulted.declarations[0].value_or_nil.data.as_deref()
        else {
            panic!("expected defaulted arrow");
        };
        assert!(defaulted.args[0].default_or_nil.data.is_some());
    }

    #[test]
    fn erases_type_script_expression_assertions() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "const typed = input as Payload;\
             const checked = config satisfies Options;\
             const angled = <Map<Key, Value>>input;\
             typed!.field;\
             (handler as Handler)(typed);\
             const sum = input as number + 1;",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Local(typed)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected typed declaration");
        };
        assert!(matches!(
            typed.declarations[0].value_or_nil.data.as_deref(),
            Some(ExprData::Identifier(_))
        ));
        let Some(StmtData::Local(checked)) = ast.parts[1].statements[1].data.as_deref() else {
            panic!("expected satisfies declaration");
        };
        assert!(matches!(
            checked.declarations[0].value_or_nil.data.as_deref(),
            Some(ExprData::Identifier(_))
        ));
        let Some(StmtData::Local(angled)) = ast.parts[1].statements[2].data.as_deref() else {
            panic!("expected angle-bracket assertion");
        };
        assert!(matches!(
            angled.declarations[0].value_or_nil.data.as_deref(),
            Some(ExprData::Identifier(_))
        ));
        assert!(matches!(
            ast.parts[1].statements[3].data.as_deref(),
            Some(StmtData::Expr(statement))
                if matches!(statement.value.data.as_deref(), Some(ExprData::Dot(_)))
        ));
        assert!(matches!(
            ast.parts[1].statements[4].data.as_deref(),
            Some(StmtData::Expr(statement))
                if matches!(statement.value.data.as_deref(), Some(ExprData::Call(_)))
        ));
        assert!(matches!(
            ast.parts[1].statements[5].data.as_deref(),
            Some(StmtData::Local(local))
                if matches!(
                    local.declarations[0].value_or_nil.data.as_deref(),
                    Some(ExprData::Binary(binary))
                        if binary.op == crate::internal::js_ast::OpCode::BinaryAdd
                )
        ));
    }

    #[test]
    fn erases_type_arguments_in_expressions_with_backtracking() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "const called = handler<Result>(input);\
             const made = new Box<Item>(input);\
             const instantiated = handler<Result>;\
             const compared = a < b > c;",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let values = ast.parts[1]
            .statements
            .iter()
            .map(|statement| {
                let Some(StmtData::Local(local)) = statement.data.as_deref() else {
                    panic!("expected local declaration");
                };
                local.declarations[0].value_or_nil.data.as_deref()
            })
            .collect::<Vec<_>>();
        assert!(matches!(values[0], Some(ExprData::Call(_))));
        assert!(matches!(values[1], Some(ExprData::New(_))));
        assert!(matches!(values[2], Some(ExprData::Identifier(_))));
        assert!(matches!(
            values[3],
            Some(ExprData::Binary(binary))
                if binary.op == crate::internal::js_ast::OpCode::BinaryGreaterThan
        ));
    }

    #[test]
    fn erases_type_only_imports_and_specifiers() {
        let mut options = Options::default();
        options.ts.parse = true;
        options.mode = crate::internal::config::Mode::Bundle;
        let (ast, ok, log) = parse_source_with_options(
            "import type {Shape} from './types';\
             import {type Hidden, value, type Other as Alias} from './runtime';\
             value;",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 3);
        assert!(ast.parts[1].statements[0].data.is_none());
        let Some(StmtData::Import(import)) = ast.parts[1].statements[1].data.as_deref() else {
            panic!("expected retained runtime import");
        };
        assert_eq!(import.items.as_ref().map_or(0, Vec::len), 1);
        assert_eq!(ast.import_records.len(), 1);
        assert_eq!(ast.import_records[0].path.text, "./runtime");
        assert_eq!(ast.named_imports.len(), 1);
        assert_eq!(
            ast.named_imports
                .values()
                .next()
                .expect("named import")
                .alias,
            "value"
        );
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);

        let mut options = Options::default();
        options.ts.parse = true;
        options.mode = crate::internal::config::Mode::Bundle;
        let (ast, ok, log) = parse_source_with_options(
            "import {type, type as kind} from './runtime'; type; kind;",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Import(import)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected runtime imports named type");
        };
        assert_eq!(import.items.as_ref().map_or(0, Vec::len), 2);
        assert_eq!(ast.named_imports.len(), 2);
        assert!(ast.named_imports.values().all(|item| item.alias == "type"));
    }

    #[test]
    fn parses_type_script_enum_declarations() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "enum Color { Red, Green = 2, Yellow = Green + 3, Blue = 'blue', Computed = make() }\
             const red = Color.Red;\
             const blue = Color.Blue;\
             const green = Color['Green'];\
             Color.Red = 3;\
             Color['Green'] = 4;\
             export const enum Flags { A = 1, B }",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.parts[1].statements.len(), 7);
        let color_ref = ast
            .ts_enums
            .keys()
            .copied()
            .find(|reference| {
                ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                    .original_name
                    == "Color"
            })
            .expect("Color enum constants");
        assert!(ast.ts_enums[&color_ref]["Red"].number.abs() < f64::EPSILON);
        assert!((ast.ts_enums[&color_ref]["Green"].number - 2.0).abs() < f64::EPSILON);
        assert!((ast.ts_enums[&color_ref]["Yellow"].number - 5.0).abs() < f64::EPSILON);
        assert_eq!(
            crate::internal::helpers::utf16_to_string(&ast.ts_enums[&color_ref]["Blue"].string),
            b"blue"
        );
        assert!(matches!(
            ast.parts[1].statements[0].data.as_deref(),
            Some(StmtData::Local(local))
                if local.kind == LocalKind::Var
                    && matches!(
                        local.declarations[0].value_or_nil.data.as_deref(),
                        Some(ExprData::Call(_))
                    )
        ));
        assert!(matches!(
            ast.parts[1].statements[1].data.as_deref(),
            Some(StmtData::Local(local))
                if matches!(
                    local.declarations[0].value_or_nil.data.as_deref(),
                    Some(ExprData::InlinedEnum(value))
                        if matches!(value.value.data.as_deref(), Some(ExprData::Number(0.0)))
                )
        ));
        assert!(matches!(
            ast.parts[1].statements[6].data.as_deref(),
            Some(StmtData::Local(local)) if local.is_export && local.kind == LocalKind::Var
        ));
        assert_eq!(ast.exports_kind, crate::internal::js_ast::ExportsKind::Esm);
        assert!(ast.named_exports.contains_key("Flags"));
        assert!(
            !ast.parts[1]
                .statements
                .iter()
                .any(|statement| matches!(statement.data.as_deref(), Some(StmtData::Enum(_))))
        );
    }

    #[test]
    fn folds_type_script_enum_constant_expressions() {
        let mut options = Options::default();
        options.ts.parse = true;
        let (ast, ok, log) = parse_source_with_options(
            "enum Folded {\
                Add = 1 + 2,\
                Sub = -1 - 2,\
                Mul = 10 * 20,\
                Div = 10 / 20,\
                Mod = 123 % 100,\
                Pow = 2.25 ** 3,\
                Complement = ~1,\
                Shift = 8 >> 2,\
                Previous = Add + 4\
            }",
            options,
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let values = ast.ts_enums.values().next().expect("Folded enum constants");
        let expected = [3.0, -3.0, 200.0, 0.5, 23.0, 11.390_625, -2.0, 2.0, 7.0];
        let names = [
            "Add",
            "Sub",
            "Mul",
            "Div",
            "Mod",
            "Pow",
            "Complement",
            "Shift",
            "Previous",
        ];
        for (name, expected) in names.into_iter().zip(expected) {
            assert!((values[name].number - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn for_loops_keep_lexical_bindings_in_a_loop_scope() {
        let (ast, ok, log) =
            parse_source("for (let item of items) { item; } item; for (let i = 0; i < 1; i++) {}");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.parts[1]
                .scopes
                .iter()
                .map(|scope| scope.lock().expect("scope lock").kind)
                .collect::<Vec<_>>(),
            [
                crate::internal::js_ast::ScopeKind::Entry,
                crate::internal::js_ast::ScopeKind::Block,
                crate::internal::js_ast::ScopeKind::Block,
                crate::internal::js_ast::ScopeKind::Block,
                crate::internal::js_ast::ScopeKind::Block,
            ]
        );

        let Some(StmtData::ForOf(loop_statement)) = ast.parts[1].statements[0].data.as_deref()
        else {
            panic!("expected for-of statement");
        };
        let Some(StmtData::Local(local)) = loop_statement.init.data.as_deref() else {
            panic!("expected loop declaration");
        };
        let Some(crate::internal::js_ast::BindingData::Identifier(binding)) =
            local.declarations[0].binding.data.as_deref()
        else {
            panic!("expected identifier binding");
        };
        let item_ref = binding.reference;
        let Some(StmtData::Block(body)) = loop_statement.body.data.as_deref() else {
            panic!("expected loop body");
        };
        let Some(StmtData::Expr(inner_use)) = body.statements[0].data.as_deref() else {
            panic!("expected inner use");
        };
        assert!(matches!(
            inner_use.value.data.as_deref(),
            Some(ExprData::Identifier(identifier)) if identifier.reference == item_ref
        ));
        let Some(StmtData::Expr(outer_use)) = ast.parts[1].statements[1].data.as_deref() else {
            panic!("expected outer use");
        };
        assert!(matches!(
            outer_use.value.data.as_deref(),
            Some(ExprData::Identifier(identifier)) if identifier.reference != item_ref
        ));
    }

    #[test]
    fn records_nested_declared_symbols_for_linking() {
        let (ast, ok, log) = parse_source(
            "function outer(arg) {\
               let local;\
               try {} catch (error) { const caught = error; }\
             }\
             for (let item of items) {}\
             if (condition) { var hoisted; }",
        );
        assert!(ok);
        assert!(log.done().is_empty());

        let declarations = ast.parts[1]
            .declared_symbols
            .iter()
            .map(|declared| {
                let name = ast.symbols
                    [usize::try_from(declared.reference.inner_index).expect("symbol index")]
                .original_name
                .as_str();
                (name, declared.is_top_level)
            })
            .collect::<HashMap<_, _>>();
        assert!(declarations["outer"]);
        assert!(declarations["hoisted"]);
        for name in ["arg", "local", "error", "caught", "item"] {
            assert!(!declarations[name], "{name}");
        }
    }

    #[test]
    fn rejects_with_statements_in_strict_mode() {
        let (_, ok, log) = parse_source("\"use strict\"; with (object) {}");
        assert!(ok);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn classifies_top_level_return_as_common_js_and_rejects_it_in_esm() {
        let (ast, ok, log) = parse_source("return;");
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(
            ast.exports_kind,
            crate::internal::js_ast::ExportsKind::CommonJs
        );

        let (_, ok, log) = parse_source("export {}; return;");
        assert!(ok);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn constructs_lazy_exports_with_global_and_runtime_helpers() {
        fn string_expression(text: &str) -> Expr {
            Expr::new(
                Loc::default(),
                ExprData::String(StringExpr {
                    value: string_to_utf16(text.as_bytes()),
                    ..StringExpr::default()
                }),
            )
        }

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            identifier_name: "data".into(),
            ..Source::default()
        };
        let global = lazy_export_ast(
            log.clone(),
            &source,
            Options::default(),
            string_expression("AA=="),
            Some(&HelperCall {
                global: vec!["Uint8Array".into(), "fromBase64".into()],
                ..HelperCall::default()
            }),
        );
        assert!(global.has_lazy_export);
        assert_eq!(global.parts.len(), 2);
        let Some(StmtData::LazyExport(export)) = global.parts[1].statements[0].data.as_deref()
        else {
            panic!("expected a lazy export");
        };
        let Some(ExprData::Call(call)) = export.value.data.as_deref() else {
            panic!("expected a helper call");
        };
        assert!(matches!(
            call.target.data.as_deref(),
            Some(ExprData::Dot(dot)) if dot.name == "fromBase64"
        ));
        assert_eq!(
            global
                .symbols
                .iter()
                .filter(|symbol| symbol.original_name == "Uint8Array")
                .count(),
            1
        );

        let runtime_ast = lazy_export_ast(
            log,
            &source,
            Options::default(),
            string_expression("AA=="),
            Some(&HelperCall {
                runtime: "__toBinary".into(),
                ..HelperCall::default()
            }),
        );
        assert_eq!(runtime_ast.parts.len(), 3);
        assert_eq!(runtime_ast.import_records.len(), 1);
        assert_eq!(
            runtime_ast.import_records[0].source_index.get_index(),
            runtime::SOURCE_INDEX
        );
        assert!(
            runtime_ast
                .named_imports
                .values()
                .any(|import| import.alias == "__toBinary")
        );
    }

    #[test]
    fn parses_static_import_attributes_and_assertions() {
        let (ast, ok, log) = parse_source(
            "import data from './data.json' with { type: 'json', mode: 'full' };\
             export {value} from './legacy.json' assert { type: 'json' };",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 2);

        let attributes = ast.import_records[0]
            .assert_or_with
            .as_ref()
            .expect("import attributes");
        assert_eq!(
            attributes.keyword,
            crate::internal::ast::AssertOrWithKeyword::With
        );
        assert_eq!(attributes.entries.len(), 2);
        assert_eq!(
            crate::internal::helpers::utf16_to_string(&attributes.entries[0].key),
            b"type"
        );
        assert!(
            !ast.import_records[0]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::ASSERT_TYPE_JSON)
        );

        let assertion = ast.import_records[1]
            .assert_or_with
            .as_ref()
            .expect("import assertion");
        assert_eq!(
            assertion.keyword,
            crate::internal::ast::AssertOrWithKeyword::Assert
        );
        assert!(
            ast.import_records[1]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::ASSERT_TYPE_JSON)
        );
    }

    #[test]
    fn reports_duplicate_static_import_attributes() {
        let (_, ok, log) = parse_source("import './data.json' with { type: 'json', type: 'json' }");
        assert!(ok);
        assert_eq!(
            log.done()[0].data.text,
            "Duplicate import attribute \"type\""
        );
    }

    #[test]
    fn extracts_dynamic_import_attributes_into_import_records() {
        let (ast, ok, log) = parse_source(
            "const data = import('./data.txt', { with: { type: 'json' } });\
             const legacy = import('./legacy.json', { assert: { type: 'json' } });\
             const runtime = import('./runtime.js', { with: { type: getType() } });",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 2);
        assert_eq!(
            ast.import_records[0]
                .assert_or_with
                .as_ref()
                .expect("dynamic import attributes")
                .keyword,
            crate::internal::ast::AssertOrWithKeyword::With
        );
        assert_eq!(
            ast.import_records[1]
                .assert_or_with
                .as_ref()
                .expect("dynamic import assertion")
                .keyword,
            crate::internal::ast::AssertOrWithKeyword::Assert
        );
        assert!(
            ast.import_records[1]
                .flags
                .contains(crate::internal::ast::ImportRecordFlags::ASSERT_TYPE_JSON)
        );

        let dynamic_values = ast.parts[1]
            .statements
            .iter()
            .map(|statement| {
                let Some(StmtData::Local(local)) = statement.data.as_deref() else {
                    panic!("expected local declaration");
                };
                local.declarations[0].value_or_nil.data.as_deref()
            })
            .collect::<Vec<_>>();
        assert!(matches!(dynamic_values[0], Some(ExprData::ImportString(_))));
        assert!(matches!(dynamic_values[1], Some(ExprData::ImportString(_))));
        assert!(matches!(dynamic_values[2], Some(ExprData::ImportCall(_))));
    }
}
