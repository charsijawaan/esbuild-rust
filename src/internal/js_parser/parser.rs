use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
};

use crate::internal::{
    ast::{Ref, SymbolKind},
    helpers::utf16_to_string,
    js_ast::{
        Ast, DeclaredSymbol, ExportsKind, ExprData, LocalKind, Part, Scope, ScopeKind, Stmt,
        StmtData, StrictModeKind, for_each_identifier_binding,
    },
    js_lexer::{Lexer, LexerPanic, Token},
    logger::{Loc, Log, Source},
};

use super::{
    Options, parser_core::ParserCore, parser_types::AwaitOrYield, syntax_statement::parse_statement,
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

        let directives = strip_directive_prologue(&core, &mut statements);
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
        let has_esm_exports = statements.iter().any(|statement| {
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
        let declared_symbols = declare_top_level_symbols(&mut core, &mut statements);
        core.prepare_for_visit_pass(has_esm_exports, has_import_statement);
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
                scopes: vec![module_scope.clone()],
                import_record_indices,
                declared_symbols,
                symbol_uses: std::mem::take(&mut core.symbol_uses),
                ..Part::default()
            });
        }

        let exports_kind = if has_esm_exports
            || has_import_statement
            || core.options.module_type_data.module_type.is_esm()
        {
            ExportsKind::Esm
        } else if core.options.module_type_data.module_type.is_common_js() {
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
            source_map_comment: lexer.source_mapping_url.clone(),
            exports_ref: core.exports_ref,
            module_ref: core.module_ref,
            wrapper_ref,
            approximate_line_count: i32::try_from(lexer.approximate_newline_count)
                .unwrap_or(i32::MAX)
                .saturating_add(1),
            exports_kind,
            ..Ast::default()
        };
    }));

    match parsed {
        Ok(()) => (result, true),
        Err(payload) if payload.downcast_ref::<LexerPanic>().is_some() => (result, false),
        Err(payload) => std::panic::resume_unwind(payload),
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
                            let name = String::from_utf8_lossy(
                                core.load_name_from_ref(identifier.reference),
                            )
                            .into_owned();
                            identifier.reference = core.declare_symbol(kind, loc, &name);
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
    let text = String::from_utf8_lossy(core.load_name_from_ref(name.reference)).into_owned();
    name.reference = core.declare_symbol(kind, name.loc, &text);
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

fn strip_directive_prologue(core: &ParserCore, statements: &mut Vec<Stmt>) -> Vec<String> {
    let mut directives = Vec::new();
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
        if !directives.contains(&directive) {
            directives.push(directive);
        }
        count += 1;
    }
    statements.drain(..count);
    directives
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse;
    use crate::internal::{
        js_ast::{ExprData, StmtData},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    fn parse_source(text: &str) -> (crate::internal::js_ast::Ast, bool, Log) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text.as_bytes()),
            identifier_name: "entry".to_owned(),
            ..Source::default()
        };
        let (ast, ok) = parse(log.clone(), source, Options::default());
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
        assert_eq!(ast.symbols.len(), 6);
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
        assert_eq!(ast.parts[1].declared_symbols.len(), 4);
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
}
