use crate::internal::{
    ast::{ImportKind, ImportPhase, ImportRecordFlags, Index32, LocRef, Ref, SymbolKind},
    config::InjectedFile,
    helpers::utf16_to_string,
    js_ast::{
        AssignTarget, ClauseItem, DeclaredSymbol, Expr, ExprData, ImportIdentifierExpr, ImportStmt,
        NamedImport, OptionalChain, Part, ScopeKind, ScopeMember, Stmt, StmtData, is_identifier,
    },
    js_lexer::range_of_identifier,
    logger::{LineColumnTracker, Loc, Range},
};

use super::parser_core::ParserCore;

pub(super) struct DotName {
    parts: Vec<String>,
    reference: Ref,
}

pub(super) fn prepare(core: &mut ParserCore) -> Vec<Part> {
    let files = core.options.injected_files.clone();
    files.iter().map(|file| prepare_file(core, file)).collect()
}

fn prepare_file(core: &mut ParserCore, file: &InjectedFile) -> Part {
    let mut imports = Vec::new();
    for export in &file.exports {
        let scope = core.module_scope.as_ref().expect("injected module scope");
        if scope
            .lock()
            .expect("scope lock")
            .members
            .contains_key(&export.alias)
        {
            continue;
        }
        let parts = export
            .alias
            .split('.')
            .map(str::to_string)
            .collect::<Vec<_>>();
        if parts.iter().any(|part| !is_identifier(part)) {
            continue;
        }
        let reference = core.new_symbol(SymbolKind::Injected, export.alias.clone());
        if parts.len() == 1 {
            core.module_scope
                .as_ref()
                .unwrap()
                .lock()
                .expect("scope lock")
                .members
                .insert(
                    export.alias.clone(),
                    ScopeMember {
                        reference,
                        ..ScopeMember::default()
                    },
                );
        } else {
            core.injected_dot_names.push(DotName { parts, reference });
        }
        core.injected_symbol_sources
            .insert(reference, (file.source.clone(), export.loc));
        imports.push((export.alias.clone(), reference));
    }
    let namespace_ref = core.new_symbol(
        SymbolKind::Other,
        format!(
            "import_{}",
            crate::internal::js_ast::generate_non_unique_name_from_path(&file.source.key_path.text)
        ),
    );
    core.module_scope
        .as_ref()
        .unwrap()
        .lock()
        .expect("scope lock")
        .generated
        .push(namespace_ref);
    let import_record_index = core.add_import_record(
        ImportKind::Stmt,
        ImportPhase::Evaluation,
        Range::default(),
        file.source.key_path.text.clone(),
        ImportRecordFlags::default(),
    );
    let record = &mut core.import_records[import_record_index as usize];
    if file.is_copy_loader {
        record.copy_source_index = Index32::new(file.source.index);
    } else {
        record.source_index = Index32::new(file.source.index);
    }
    core.injected_import_records.push(import_record_index);
    let (items, declared_symbols) = import_items(core, imports, namespace_ref, import_record_index);
    Part {
        statements: vec![Stmt::new(
            Loc::default(),
            StmtData::Import(ImportStmt {
                items: (!items.is_empty()).then_some(items),
                namespace_ref,
                import_record_index,
                is_single_line: true,
                ..ImportStmt::default()
            }),
        )],
        declared_symbols,
        import_record_indices: vec![import_record_index],
        ..Part::default()
    }
}

fn import_items(
    core: &mut ParserCore,
    imports: Vec<(String, Ref)>,
    namespace_ref: Ref,
    import_record_index: u32,
) -> (Vec<ClauseItem>, Vec<DeclaredSymbol>) {
    let mut declared_symbols = vec![DeclaredSymbol {
        reference: namespace_ref,
        is_top_level: true,
    }];
    let items = imports
        .into_iter()
        .map(|(alias, reference)| {
            core.is_import_item.insert(reference);
            core.generated_named_imports.insert(
                reference,
                NamedImport {
                    alias: alias.clone(),
                    namespace_ref,
                    import_record_index,
                    ..NamedImport::default()
                },
            );
            declared_symbols.push(DeclaredSymbol {
                reference,
                is_top_level: true,
            });
            ClauseItem {
                alias,
                name: LocRef {
                    reference,
                    ..LocRef::default()
                },
                ..ClauseItem::default()
            }
        })
        .collect();
    (items, declared_symbols)
}

fn matches_parts(core: &ParserCore, expression: &Expr, parts: &[String]) -> bool {
    match expression.data.as_deref() {
        Some(ExprData::Dot(dot)) if dot.optional_chain == OptionalChain::None => {
            parts.last().is_some_and(|part| part == &dot.name)
                && matches_parts(core, &dot.target, &parts[..parts.len() - 1])
        }
        Some(ExprData::Index(index)) if index.optional_chain == OptionalChain::None => {
            let Some(ExprData::String(string)) = index.index.data.as_deref() else {
                return false;
            };
            parts
                .last()
                .is_some_and(|part| part.as_bytes() == utf16_to_string(&string.value))
                && matches_parts(core, &index.target, &parts[..parts.len() - 1])
        }
        Some(ExprData::ImportMeta(_)) => parts == ["import", "meta"],
        Some(ExprData::ImportIdentifier(identifier)) if parts.len() == 1 => {
            let symbol = &core.symbols[identifier.reference.inner_index as usize];
            symbol.original_name == parts[0] && symbol.kind.is_unbound_or_injected()
        }
        Some(ExprData::Identifier(identifier)) if parts.len() == 1 => {
            let name = if ParserCore::is_stored_name_ref(identifier.reference) {
                String::from_utf8_lossy(core.load_name_from_ref(identifier.reference)).into_owned()
            } else {
                core.symbols[identifier.reference.inner_index as usize]
                    .original_name
                    .clone()
            };
            if name != parts[0] || identifier.must_keep_due_to_with_stmt {
                return false;
            }
            let mut scope = core.current_scope.clone();
            while let Some(current) = scope {
                let current = current.lock().expect("scope lock");
                if current.kind == ScopeKind::With {
                    return false;
                }
                if let Some(member) = current.members.get(&name) {
                    let reference = core.follow_symbol_link(member.reference);
                    return core.symbols[reference.inner_index as usize]
                        .kind
                        .is_unbound_or_injected();
                }
                scope = current.parent.as_ref().and_then(std::sync::Weak::upgrade);
            }
            true
        }
        _ => false,
    }
}

pub(super) fn rewrite(
    core: &mut ParserCore,
    expression: &Expr,
    target: AssignTarget,
) -> Option<ExprData> {
    let is_dot = matches!(
        expression.data.as_deref(),
        Some(ExprData::Dot(_) | ExprData::ImportMeta(_))
    ) || core.options.minify_syntax
        && matches!(expression.data.as_deref(), Some(ExprData::Index(index))
                if matches!(index.index.data.as_deref(), Some(ExprData::String(string))
                    if is_identifier(&String::from_utf8_lossy(&utf16_to_string(&string.value)))));
    if !is_dot {
        return None;
    }
    let matched = core
        .injected_dot_names
        .iter()
        .find(|name| matches_parts(core, expression, &name.parts))?;
    let tail = matched.parts.last()?.clone();
    let define = core
        .options
        .defines
        .as_ref()
        .and_then(|defines| defines.dot_defines.get(&tail))
        .and_then(|defines| {
            defines
                .iter()
                .find(|define| matches_parts(core, expression, &define.key_parts))
        })
        .and_then(|define| define.define_expr.clone());
    if target == AssignTarget::None
        && let Some(define) = define
    {
        return super::visit::instantiate_define_expr(core, expression.loc, &define);
    }
    rewrite_injected(core, expression, target)
}

pub(super) fn rewrite_injected(
    core: &mut ParserCore,
    expression: &Expr,
    target: AssignTarget,
) -> Option<ExprData> {
    if core.injected_dot_names.is_empty() {
        return None;
    }
    let reference = core
        .injected_dot_names
        .iter()
        .find(|name| matches_parts(core, expression, &name.parts))?
        .reference;
    if target != AssignTarget::None {
        assignment_error(core, expression.loc, reference);
    }
    core.record_usage(reference);
    Some(ExprData::ImportIdentifier(ImportIdentifierExpr {
        reference,
        ..ImportIdentifierExpr::default()
    }))
}

pub(super) fn assignment_error(core: &mut ParserCore, loc: Loc, reference: Ref) {
    let Some((source, exported_loc)) = core.injected_symbol_sources.get(&reference).cloned() else {
        return;
    };
    let name = &core.symbols[reference.inner_index as usize].original_name;
    let mut tracker = LineColumnTracker::new(Some(&source));
    let note = tracker.msg_data(
        range_of_identifier(&source, exported_loc),
        format!(
            "The symbol {name:?} was exported from {:?} here:",
            source.pretty_paths.select(core.options.log_path_style)
        ),
    );
    if let Some(log) = &core.log {
        log.add_error_with_notes(
            Some(&mut core.tracker),
            range_of_identifier(&core.source, loc),
            format!("Cannot assign to {name:?} because it's an import from an injected file"),
            vec![note],
        );
    }
}
