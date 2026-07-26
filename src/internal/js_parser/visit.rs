#![allow(dead_code)]

use crate::internal::js_ast::{
    Binding, BindingData, BlockStmt, Class, Expr, ExprData, Function, PropertyFlags, ScopeKind,
    Stmt, StmtData,
};

use super::parser_core::ParserCore;

pub(crate) fn visit_top_level_statements(core: &mut ParserCore, statements: &mut [Stmt]) {
    visit_statements(core, statements, true);
}

#[allow(clippy::too_many_lines)]
fn visit_statements(core: &mut ParserCore, statements: &mut [Stmt], resolve_identifiers: bool) {
    for statement in statements {
        match statement.data.as_deref_mut() {
            Some(StmtData::Block(block)) => {
                visit_block(core, statement.loc, block, resolve_identifiers);
            }
            Some(StmtData::Expr(expression)) => {
                visit_expr(core, &mut expression.value, resolve_identifiers);
            }
            Some(StmtData::Local(local)) => {
                for declaration in &mut local.declarations {
                    visit_binding_initializers(core, &mut declaration.binding, resolve_identifiers);
                    visit_expr(core, &mut declaration.value_or_nil, resolve_identifiers);
                }
            }
            Some(StmtData::Function(function)) => {
                visit_function(core, &mut function.function, resolve_identifiers);
            }
            Some(StmtData::Class(class)) => {
                visit_class(core, &mut class.class, resolve_identifiers);
            }
            Some(StmtData::ExportDefault(export)) => match export.value.data.as_deref_mut() {
                Some(StmtData::Expr(expression)) => {
                    visit_expr(core, &mut expression.value, resolve_identifiers);
                }
                Some(StmtData::Function(function)) => {
                    visit_function(core, &mut function.function, resolve_identifiers);
                }
                Some(StmtData::Class(class)) => {
                    visit_class(core, &mut class.class, resolve_identifiers);
                }
                _ => {}
            },
            Some(StmtData::If(if_statement)) => {
                visit_expr(core, &mut if_statement.test, resolve_identifiers);
                visit_statement(core, &mut if_statement.yes, resolve_identifiers);
                visit_statement(core, &mut if_statement.no_or_nil, resolve_identifiers);
            }
            Some(StmtData::DoWhile(loop_statement)) => {
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                visit_expr(core, &mut loop_statement.test, resolve_identifiers);
            }
            Some(StmtData::While(loop_statement)) => {
                visit_expr(core, &mut loop_statement.test, resolve_identifiers);
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
            }
            Some(StmtData::With(with_statement)) => {
                visit_expr(core, &mut with_statement.value, resolve_identifiers);
                visit_statement(core, &mut with_statement.body, resolve_identifiers);
            }
            Some(StmtData::Throw(throw_statement)) => {
                visit_expr(core, &mut throw_statement.value, resolve_identifiers);
            }
            Some(StmtData::Return(return_statement)) => {
                visit_expr(
                    core,
                    &mut return_statement.value_or_nil,
                    resolve_identifiers,
                );
            }
            Some(StmtData::For(loop_statement)) => {
                visit_statement(core, &mut loop_statement.init_or_nil, resolve_identifiers);
                visit_expr(core, &mut loop_statement.test_or_nil, resolve_identifiers);
                visit_expr(core, &mut loop_statement.update_or_nil, resolve_identifiers);
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
            }
            Some(StmtData::ForIn(loop_statement)) => {
                visit_statement(core, &mut loop_statement.init, resolve_identifiers);
                visit_expr(core, &mut loop_statement.value, resolve_identifiers);
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
            }
            Some(StmtData::ForOf(loop_statement)) => {
                visit_statement(core, &mut loop_statement.init, resolve_identifiers);
                visit_expr(core, &mut loop_statement.value, resolve_identifiers);
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
            }
            Some(StmtData::Label(label)) => {
                core.push_scope_for_visit_pass(ScopeKind::Label, statement.loc);
                let name = String::from_utf8_lossy(core.load_name_from_ref(label.name.reference))
                    .into_owned();
                let reference = core.new_symbol(crate::internal::ast::SymbolKind::Label, name);
                label.name.reference = reference;
                {
                    let mut scope = core
                        .current_scope
                        .as_ref()
                        .expect("label scope")
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    scope.label = crate::internal::ast::LocRef {
                        loc: label.name.loc,
                        reference,
                    };
                    scope.label_stmt_is_loop = matches!(
                        label.statement.data.as_deref(),
                        Some(
                            StmtData::For(_)
                                | StmtData::ForIn(_)
                                | StmtData::ForOf(_)
                                | StmtData::While(_)
                                | StmtData::DoWhile(_)
                        )
                    );
                }
                visit_statement(core, &mut label.statement, resolve_identifiers);
                core.pop_scope();
            }
            Some(StmtData::Switch(switch)) => {
                visit_expr(core, &mut switch.test, resolve_identifiers);
                for case in &mut switch.cases {
                    visit_expr(core, &mut case.value_or_nil, resolve_identifiers);
                    visit_statements(core, &mut case.body, resolve_identifiers);
                }
            }
            Some(StmtData::Try(try_statement)) => {
                visit_block(
                    core,
                    try_statement.block_loc,
                    &mut try_statement.block,
                    resolve_identifiers,
                );
                if let Some(catch) = &mut try_statement.catch {
                    core.push_scope_for_visit_pass(ScopeKind::CatchBinding, catch.loc);
                    visit_binding_initializers(
                        core,
                        &mut catch.binding_or_nil,
                        resolve_identifiers,
                    );
                    visit_block(core, catch.block_loc, &mut catch.block, resolve_identifiers);
                    core.pop_scope();
                }
                if let Some(finally) = &mut try_statement.finally {
                    core.push_next_scope_for_visit_pass(ScopeKind::Block);
                    visit_statements(core, &mut finally.block.statements, resolve_identifiers);
                    core.pop_scope();
                }
            }
            Some(StmtData::Break(break_statement)) => {
                bind_label_reference(core, &mut break_statement.label, false);
            }
            Some(StmtData::Continue(continue_statement)) => {
                bind_label_reference(core, &mut continue_statement.label, true);
            }
            _ => {}
        }
    }
}

fn bind_label_reference(
    core: &mut ParserCore,
    label: &mut Option<crate::internal::ast::LocRef>,
    must_be_loop: bool,
) {
    let Some(label) = label else {
        return;
    };
    if !ParserCore::is_stored_name_ref(label.reference) {
        return;
    }
    let name = String::from_utf8_lossy(core.load_name_from_ref(label.reference)).into_owned();
    let (reference, is_loop, found) = core.find_label_symbol(label.loc, &name);
    label.reference = reference;
    if found && must_be_loop && !is_loop {
        core.add_error_range(
            crate::internal::js_lexer::range_of_identifier(&core.source, label.loc),
            format!("Cannot continue to label {name:?}"),
        );
    }
}

fn visit_statement(core: &mut ParserCore, statement: &mut Stmt, resolve_identifiers: bool) {
    visit_statements(core, std::slice::from_mut(statement), resolve_identifiers);
}

fn visit_block(
    core: &mut ParserCore,
    loc: crate::internal::logger::Loc,
    block: &mut BlockStmt,
    resolve_identifiers: bool,
) {
    core.push_scope_for_visit_pass(ScopeKind::Block, loc);
    visit_statements(core, &mut block.statements, resolve_identifiers);
    core.pop_scope();
}

fn visit_function(core: &mut ParserCore, function: &mut Function, resolve_identifiers: bool) {
    core.push_scope_for_visit_pass(ScopeKind::FunctionArgs, function.open_paren_loc);
    for argument in &mut function.args {
        visit_binding_initializers(core, &mut argument.binding, resolve_identifiers);
        visit_expr(core, &mut argument.default_or_nil, resolve_identifiers);
        for decorator in &mut argument.decorators {
            visit_expr(core, &mut decorator.value, resolve_identifiers);
        }
    }
    core.push_scope_for_visit_pass(ScopeKind::FunctionBody, function.body.loc);
    visit_statements(
        core,
        &mut function.body.block.statements,
        resolve_identifiers,
    );
    core.pop_scope();
    core.pop_scope();
}

fn visit_class(core: &mut ParserCore, class: &mut Class, resolve_identifiers: bool) {
    for decorator in &mut class.decorators {
        visit_expr(core, &mut decorator.value, resolve_identifiers);
    }
    visit_expr(core, &mut class.extends_or_nil, resolve_identifiers);
    for property in &mut class.properties {
        if property.flags.contains(PropertyFlags::IS_COMPUTED) {
            visit_expr(core, &mut property.key, resolve_identifiers);
        }
        for decorator in &mut property.decorators {
            visit_expr(core, &mut decorator.value, resolve_identifiers);
        }
        visit_expr(core, &mut property.value_or_nil, resolve_identifiers);
        visit_expr(core, &mut property.initializer_or_nil, resolve_identifiers);
        if let Some(static_block) = &mut property.class_static_block {
            visit_block(
                core,
                static_block.loc,
                &mut static_block.block,
                resolve_identifiers,
            );
        }
    }
}

fn visit_binding_initializers(
    core: &mut ParserCore,
    binding: &mut Binding,
    resolve_identifiers: bool,
) {
    match binding.data.as_deref_mut() {
        Some(BindingData::Array(array)) => {
            for item in &mut array.items {
                visit_binding_initializers(core, &mut item.binding, resolve_identifiers);
                visit_expr(core, &mut item.default_value_or_nil, resolve_identifiers);
            }
        }
        Some(BindingData::Object(object)) => {
            for property in &mut object.properties {
                if property.is_computed {
                    visit_expr(core, &mut property.key, resolve_identifiers);
                }
                visit_binding_initializers(core, &mut property.value, resolve_identifiers);
                visit_expr(
                    core,
                    &mut property.default_value_or_nil,
                    resolve_identifiers,
                );
            }
        }
        Some(BindingData::Missing | BindingData::Identifier(_)) | None => {}
    }
}

#[allow(clippy::too_many_lines)]
fn visit_expr(core: &mut ParserCore, expression: &mut Expr, resolve_identifiers: bool) {
    let Some(data) = expression.data.as_deref_mut() else {
        return;
    };
    match data {
        ExprData::Identifier(identifier) => {
            if !resolve_identifiers {
                return;
            }
            if ParserCore::is_stored_name_ref(identifier.reference) {
                let name = String::from_utf8_lossy(core.load_name_from_ref(identifier.reference))
                    .into_owned();
                let result = core.find_symbol(expression.loc, &name);
                identifier.reference = result.reference;
                identifier.must_keep_due_to_with_stmt = result.is_inside_with_scope;
            } else {
                core.record_usage(identifier.reference);
            }
        }
        ExprData::ImportIdentifier(identifier) => core.record_usage(identifier.reference),
        ExprData::Array(array) => {
            for item in &mut array.items {
                visit_expr(core, item, resolve_identifiers);
            }
        }
        ExprData::Unary(unary) => visit_expr(core, &mut unary.value, resolve_identifiers),
        ExprData::Binary(binary) => {
            visit_expr(core, &mut binary.left, resolve_identifiers);
            visit_expr(core, &mut binary.right, resolve_identifiers);
        }
        ExprData::New(new) => {
            visit_expr(core, &mut new.target, resolve_identifiers);
            for argument in &mut new.args {
                visit_expr(core, argument, resolve_identifiers);
            }
        }
        ExprData::Call(call) => {
            visit_expr(core, &mut call.target, resolve_identifiers);
            for argument in &mut call.args {
                visit_expr(core, argument, resolve_identifiers);
            }
            let kind = if is_identifier_named(core, &call.target, "require") {
                Some(crate::internal::ast::ImportKind::Require)
            } else if let Some(ExprData::Dot(dot)) = call.target.data.as_deref()
                && dot.name == "resolve"
                && is_identifier_named(core, &dot.target, "require")
            {
                Some(crate::internal::ast::ImportKind::RequireResolve)
            } else {
                None
            };
            if let Some(kind) = kind
                && call.args.len() == 1
                && let Some(ExprData::String(path)) = call.args[0].data.as_deref()
            {
                let import_record_index = core.add_import_record(
                    kind,
                    crate::internal::ast::ImportPhase::Evaluation,
                    core.source.range_of_string(call.args[0].loc),
                    String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                        &path.value,
                    ))
                    .into_owned(),
                    crate::internal::ast::ImportRecordFlags::default(),
                );
                *data = if kind == crate::internal::ast::ImportKind::Require {
                    ExprData::RequireString(crate::internal::js_ast::RequireStringExpr {
                        import_record_index,
                        close_paren_loc: call.close_paren_loc,
                    })
                } else {
                    ExprData::RequireResolveString(
                        crate::internal::js_ast::RequireResolveStringExpr {
                            import_record_index,
                            close_paren_loc: call.close_paren_loc,
                        },
                    )
                };
            }
        }
        ExprData::Dot(dot) => visit_expr(core, &mut dot.target, resolve_identifiers),
        ExprData::Index(index) => {
            visit_expr(core, &mut index.target, resolve_identifiers);
            visit_expr(core, &mut index.index, resolve_identifiers);
        }
        ExprData::Object(object) => {
            for property in &mut object.properties {
                if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                    visit_expr(core, &mut property.key, resolve_identifiers);
                }
                visit_expr(core, &mut property.value_or_nil, resolve_identifiers);
                visit_expr(core, &mut property.initializer_or_nil, resolve_identifiers);
                for decorator in &mut property.decorators {
                    visit_expr(core, &mut decorator.value, resolve_identifiers);
                }
            }
        }
        ExprData::Spread(spread) => visit_expr(core, &mut spread.value, resolve_identifiers),
        ExprData::Template(template) => {
            visit_expr(core, &mut template.tag_or_nil, resolve_identifiers);
            for part in &mut template.parts {
                visit_expr(core, &mut part.value, resolve_identifiers);
            }
        }
        ExprData::InlinedEnum(inlined) => {
            visit_expr(core, &mut inlined.value, resolve_identifiers);
        }
        ExprData::Annotation(annotation) => {
            visit_expr(core, &mut annotation.value, resolve_identifiers);
        }
        ExprData::Await(await_expression) => {
            visit_expr(core, &mut await_expression.value, resolve_identifiers);
        }
        ExprData::Yield(yield_expression) => {
            visit_expr(
                core,
                &mut yield_expression.value_or_nil,
                resolve_identifiers,
            );
        }
        ExprData::If(if_expression) => {
            visit_expr(core, &mut if_expression.test, resolve_identifiers);
            visit_expr(core, &mut if_expression.yes, resolve_identifiers);
            visit_expr(core, &mut if_expression.no, resolve_identifiers);
        }
        ExprData::ImportCall(import) => {
            visit_expr(core, &mut import.expr, resolve_identifiers);
            visit_expr(core, &mut import.options_or_nil, resolve_identifiers);
            if let Some(ExprData::String(path)) = import.expr.data.as_deref() {
                let import_record_index = core.add_import_record(
                    crate::internal::ast::ImportKind::Dynamic,
                    import.phase,
                    core.source.range_of_string(import.expr.loc),
                    String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                        &path.value,
                    ))
                    .into_owned(),
                    crate::internal::ast::ImportRecordFlags::default(),
                );
                *data = ExprData::ImportString(crate::internal::js_ast::ImportStringExpr {
                    import_record_index,
                    close_paren_loc: import.close_paren_loc,
                });
            }
        }
        ExprData::Function(function) => {
            visit_function(core, &mut function.function, resolve_identifiers);
        }
        ExprData::Class(class) => visit_class(core, &mut class.class, resolve_identifiers),
        ExprData::Arrow(arrow) => {
            for argument in &mut arrow.args {
                visit_binding_initializers(core, &mut argument.binding, false);
                visit_expr(core, &mut argument.default_or_nil, false);
            }
            core.push_next_scope_for_visit_pass(ScopeKind::FunctionArgs);
            core.push_scope_for_visit_pass(ScopeKind::FunctionBody, arrow.body.loc);
            visit_statements(core, &mut arrow.body.block.statements, resolve_identifiers);
            core.pop_scope();
            core.pop_scope();
        }
        ExprData::Boolean(_)
        | ExprData::Super
        | ExprData::Null
        | ExprData::Undefined
        | ExprData::This
        | ExprData::NewTarget(_)
        | ExprData::ImportMeta(_)
        | ExprData::PrivateIdentifier(_)
        | ExprData::NameOfSymbol(_)
        | ExprData::JsxElement(_)
        | ExprData::JsxText(_)
        | ExprData::Missing
        | ExprData::Number(_)
        | ExprData::BigInt(_)
        | ExprData::String(_)
        | ExprData::RegExp(_)
        | ExprData::RequireString(_)
        | ExprData::RequireResolveString(_)
        | ExprData::ImportString(_) => {}
    }
}

fn is_identifier_named(core: &ParserCore, expression: &Expr, expected: &str) -> bool {
    let Some(ExprData::Identifier(identifier)) = expression.data.as_deref() else {
        return false;
    };
    core.symbols
        .get(usize::try_from(identifier.reference.inner_index).unwrap_or(usize::MAX))
        .is_some_and(|symbol| symbol.original_name == expected)
}
