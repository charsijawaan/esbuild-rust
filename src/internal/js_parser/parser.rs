use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
};

use crate::internal::{
    ast::{ImportRecordFlags, Ref, SymbolKind},
    helpers::utf16_to_string,
    js_ast::{
        Ast, DeclaredSymbol, ExportsKind, ExprData, LocalKind, NamedExport, NamedImport, Part,
        Scope, ScopeKind, Stmt, StmtData, StrictModeKind, for_each_identifier_binding,
    },
    js_lexer::{Lexer, LexerPanic, Token},
    logger::{Loc, Log, Source},
};

use super::{
    Options, parser_core::ParserCore, parser_types::AwaitOrYield,
    syntax_statement::parse_statement, visit::visit_top_level_statements,
};

const MODULE_SCOPE_LOC: Loc = Loc { start: -1 };

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

        let mut statements = Vec::new();
        while lexer.token != Token::EndOfFile {
            statements.push(parse_statement(&mut core, &mut lexer));
        }

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
        core.declared_symbols = declare_top_level_symbols(&mut core, &mut statements);
        core.hoist_symbols();
        let scopes = core.scope_refs_in_order();
        core.prepare_for_visit_pass(has_esm_exports, has_import_statement);
        visit_top_level_statements(&mut core, &mut statements);
        assert_eq!(
            core.remaining_scope_count(),
            0,
            "visit pass must consume every parse-pass scope"
        );
        let uses_exports_ref = core
            .symbol_uses
            .get(&core.exports_ref)
            .is_some_and(|usage| usage.count_estimate > 0);
        let uses_module_ref = core
            .symbol_uses
            .get(&core.module_ref)
            .is_some_and(|usage| usage.count_estimate > 0);
        let module_metadata = scan_module_metadata(&mut core, &mut statements);
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

        let mut parts = vec![Part {
            symbol_uses: HashMap::new(),
            can_be_removed_if_unused: true,
            ..Part::default()
        }];
        if !statements.is_empty() {
            let import_record_indices = (0..core.import_records.len())
                .map(|index| u32::try_from(index).expect("import record count fits in u32"))
                .collect();
            parts.push(Part {
                statements,
                scopes,
                import_record_indices,
                declared_symbols: std::mem::take(&mut core.declared_symbols),
                symbol_uses: std::mem::take(&mut core.symbol_uses),
                ..Part::default()
            });
        }

        let exports_kind = if has_esm_exports
            || has_import_statement
            || core.options.module_type_data.module_type.is_esm()
        {
            ExportsKind::Esm
        } else if core.options.module_type_data.module_type.is_common_js()
            || core.has_top_level_return
            || uses_exports_ref
            || uses_module_ref
        {
            ExportsKind::CommonJs
        } else {
            ExportsKind::None
        };
        let mut top_level_symbol_to_parts_from_parser = HashMap::new();
        top_level_symbol_to_parts_from_parser.insert(core.exports_ref, vec![0]);

        result = Ast {
            module_type_data: core.options.module_type_data,
            parts,
            symbols: core.symbols,
            module_scope: Some(module_scope),
            hashbang,
            directives,
            top_level_symbol_to_parts_from_parser,
            mangled_props: core.mangled_props,
            reserved_props: core.reserved_props,
            import_records: core.import_records,
            named_imports: module_metadata.named_imports,
            named_exports: module_metadata.named_exports,
            export_star_import_records: module_metadata.export_star_import_records,
            source_map_comment: lexer.source_mapping_url.clone(),
            top_level_await_keyword: core.top_level_await_keyword,
            live_top_level_await_keyword: core.top_level_await_keyword,
            exports_ref: core.exports_ref,
            module_ref: core.module_ref,
            wrapper_ref,
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

#[allow(clippy::too_many_lines)]
fn scan_module_metadata(core: &mut ParserCore, statements: &mut [Stmt]) -> ModuleMetadata {
    let mut metadata = ModuleMetadata::default();
    for statement in statements {
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
            _ => {}
        }
    }
    metadata
        .named_imports
        .extend(std::mem::take(&mut core.generated_named_imports));
    metadata
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
            Some(StmtData::Import(import)) => {
                if let Some(name) = &mut import.default_name {
                    bind_loc_ref(core, name, SymbolKind::Import, &mut declared);
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
                    }
                }
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

    let mut count = 0;
    for statement in statements.iter() {
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
        count += 1;
    }
    statements.drain(..count);
    (directives, legacy_octal_locs)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse;
    use crate::internal::{
        js_ast::{ExprData, StmtData},
        js_parser::Options,
        logger::{DeferLogKind, Log, MsgKind, Source},
    };

    fn parse_source(text: &str) -> (crate::internal::js_ast::Ast, bool, Log) {
        parse_source_with_options(text, Options::default())
    }

    fn parse_source_with_options(
        text: &str,
        options: Options,
    ) -> (crate::internal::js_ast::Ast, bool, Log) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text.as_bytes()),
            identifier_name: "entry".to_owned(),
            ..Source::default()
        };
        let (ast, ok) = parse(log.clone(), source, options);
        (ast, ok, log)
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
        assert!(matches!(
            element.tag_or_nil.data.as_deref(),
            Some(ExprData::Identifier(_))
        ));
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
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
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

        let (ast, ok, log) = parse_source_with_options("<div {...props} key={id} />;", options);
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 1);
        assert_eq!(ast.import_records[0].path.text, "react");
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        let Some(ExprData::Call(call)) = statement.value.data.as_deref() else {
            panic!("expected createElement fallback");
        };
        let Some(ExprData::ImportIdentifier(target)) = call.target.data.as_deref() else {
            panic!("expected createElement import target");
        };
        assert_eq!(ast.named_imports[&target.reference].alias, "createElement");
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
}
