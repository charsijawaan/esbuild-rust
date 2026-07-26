#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::internal::logger::{Loc, Range};
use crate::internal::{
    ast::{
        AssertOrWithEntry, AssertOrWithKeyword, ImportAssertOrWith, ImportRecordFlags, SymbolKind,
    },
    helpers::{string_to_utf16, utf16_to_string},
    js_ast::{
        AssignTarget, BinaryExpr, Binding, BindingData, BlockStmt, CallExpr, CallKind, Class,
        ClassStaticBlock, DotExpr, Expr, ExprData, ExprStmt, ForStmt, Function, FunctionExpr,
        IdentifierExpr, IfExpr, ObjectExpr, OpCode, Property, PropertyFlags, PropertyKind,
        ScopeKind, Stmt, StmtData, StrictModeKind, StringExpr, for_each_identifier_binding,
        is_identifier_es5_and_es_next, make_helper_context,
    },
};

use super::duplicate_properties::{DuplicatePropertiesIn, find_duplicate_properties};
use super::{parser_core::ParserCore, standalone_helpers::is_simple_parameter_list};

fn symbol_name(core: &ParserCore, reference: crate::internal::ast::Ref) -> String {
    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
        .original_name
        .clone()
}

fn inferred_name_from_expression(core: &ParserCore, expression: &Expr) -> Option<String> {
    match expression.data.as_deref() {
        Some(ExprData::Identifier(identifier)) => Some(symbol_name(core, identifier.reference)),
        Some(ExprData::PrivateIdentifier(private)) => Some(symbol_name(core, private.reference)),
        Some(ExprData::Dot(dot)) => Some(dot.name.clone()),
        Some(ExprData::String(string)) => {
            Some(String::from_utf8_lossy(&utf16_to_string(&string.value)).into_owned())
        }
        Some(ExprData::Index(index)) => match index.index.data.as_deref() {
            Some(ExprData::String(string)) => {
                Some(String::from_utf8_lossy(&utf16_to_string(&string.value)).into_owned())
            }
            _ => None,
        },
        _ => None,
    }
}

fn inferred_name_from_binding(core: &ParserCore, binding: &Binding) -> Option<String> {
    match binding.data.as_deref() {
        Some(BindingData::Identifier(identifier)) => Some(symbol_name(core, identifier.reference)),
        _ => None,
    }
}

fn class_has_static_name(class: &Class) -> bool {
    class.properties.iter().any(|property| {
        property.flags.contains(PropertyFlags::IS_STATIC)
            && matches!(
                property.key.data.as_deref(),
                Some(ExprData::String(value)) if utf16_to_string(&value.value) == b"name"
            )
    })
}

fn insert_class_name_static_block(core: &mut ParserCore, class: &mut Class, name: &str) -> bool {
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
                    value: crate::internal::helpers::string_to_utf16(name.as_bytes()),
                    ..StringExpr::default()
                }),
            ),
        ],
    );
    class.properties.insert(
        0,
        crate::internal::js_ast::Property {
            class_static_block: Some(Box::new(crate::internal::js_ast::ClassStaticBlock {
                block: BlockStmt {
                    statements: vec![Stmt::new(
                        loc,
                        StmtData::Expr(crate::internal::js_ast::ExprStmt {
                            value: call,
                            is_from_class_or_fn_that_can_be_removed_if_unused: true,
                        }),
                    )],
                    ..BlockStmt::default()
                },
                loc,
            })),
            loc,
            kind: PropertyKind::ClassStaticBlock,
            ..crate::internal::js_ast::Property::default()
        },
    );
    true
}

fn keep_inferred_name(core: &mut ParserCore, expression: &mut Expr, name: Option<String>) {
    let Some(name) = name else {
        return;
    };
    if !core.options.keep_names {
        return;
    }
    if let Some(ExprData::Class(class)) = expression.data.as_deref_mut() {
        if class.class.name.is_none() {
            insert_class_name_static_block(core, &mut class.class, &name);
        }
        return;
    }
    let can_keep_name = match expression.data.as_deref() {
        Some(ExprData::Function(function)) => function.function.name.is_none(),
        Some(ExprData::Arrow(_)) => true,
        _ => false,
    };
    if !can_keep_name {
        return;
    }
    let loc = expression.loc;
    let mut call = core.call_runtime(
        loc,
        "__name",
        vec![
            std::mem::take(expression),
            Expr::new(
                loc,
                ExprData::String(StringExpr {
                    value: crate::internal::helpers::string_to_utf16(name.as_bytes()),
                    ..StringExpr::default()
                }),
            ),
        ],
    );
    if let Some(ExprData::Call(call)) = call.data.as_deref_mut() {
        call.can_be_unwrapped_if_unused = true;
    }
    *expression = call;
}

pub(crate) fn visit_top_level_statements(core: &mut ParserCore, statements: &mut [Stmt]) {
    visit_statements(core, statements, true);
}

#[allow(clippy::too_many_lines)]
fn visit_statements(core: &mut ParserCore, statements: &mut [Stmt], resolve_identifiers: bool) {
    let old_control_flow_dead = core.is_control_flow_dead;
    for statement in statements.iter_mut() {
        let was_control_flow_dead = core.is_control_flow_dead;
        match statement.data.as_deref_mut() {
            Some(StmtData::Block(block)) => {
                visit_block(core, statement.loc, block, resolve_identifiers);
            }
            Some(StmtData::Expr(expression)) => {
                visit_expr(core, &mut expression.value, resolve_identifiers);
            }
            Some(StmtData::ExportEquals(export)) => {
                core.record_usage(core.module_ref);
                visit_expr(core, &mut export.value, resolve_identifiers);
            }
            Some(StmtData::LazyExport(export)) => {
                visit_expr(core, &mut export.value, resolve_identifiers);
            }
            Some(StmtData::Local(local)) => {
                for declaration in &mut local.declarations {
                    record_binding(core, &mut declaration.binding);
                    visit_binding_initializers(core, &mut declaration.binding, resolve_identifiers);
                    visit_expr(core, &mut declaration.value_or_nil, resolve_identifiers);
                    if core.options.minify_syntax
                        && local.kind == crate::internal::js_ast::LocalKind::Const
                        && let Some(BindingData::Identifier(identifier)) =
                            declaration.binding.data.as_deref()
                    {
                        let value =
                            crate::internal::js_ast::expr_to_const_value(&declaration.value_or_nil);
                        if value.kind != crate::internal::js_ast::ConstValueKind::None {
                            core.const_values.insert(identifier.reference, value);
                        }
                    }
                }
            }
            Some(StmtData::Function(function)) => {
                visit_function(core, &mut function.function, resolve_identifiers);
            }
            Some(StmtData::Class(class)) => {
                visit_class(core, &mut class.class, resolve_identifiers);
            }
            Some(StmtData::Enum(enumeration)) => {
                core.record_declared_symbol(enumeration.name.reference);
                core.record_declared_symbol(enumeration.argument);
                core.push_scope_for_visit_pass(ScopeKind::Entry, statement.loc);
                let mut next_numeric_value = 0.0;
                let mut has_numeric_value = true;
                let mut constants = HashMap::new();
                let old_should_fold = core.should_fold_type_script_constant_expressions;
                core.should_fold_type_script_constant_expressions = true;
                for value in &mut enumeration.values {
                    if value.reference != crate::internal::ast::INVALID_REF {
                        core.record_declared_symbol(value.reference);
                    }
                    visit_expr(core, &mut value.value_or_nil, resolve_identifiers);
                    let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                        &value.name,
                    ))
                    .into_owned();
                    match value.value_or_nil.data.as_deref() {
                        Some(ExprData::Number(number)) => {
                            let constant = crate::internal::js_ast::TsEnumValue {
                                number: *number,
                                ..crate::internal::js_ast::TsEnumValue::default()
                            };
                            if value.reference != crate::internal::ast::INVALID_REF {
                                core.ts_enum_values_by_ref
                                    .insert(value.reference, constant.clone());
                            }
                            constants.insert(name, constant);
                            next_numeric_value = *number + 1.0;
                            has_numeric_value = true;
                        }
                        Some(ExprData::String(string)) => {
                            let constant = crate::internal::js_ast::TsEnumValue {
                                string: string.value.clone(),
                                is_string: true,
                                ..crate::internal::js_ast::TsEnumValue::default()
                            };
                            if value.reference != crate::internal::ast::INVALID_REF {
                                core.ts_enum_values_by_ref
                                    .insert(value.reference, constant.clone());
                            }
                            constants.insert(name, constant);
                            has_numeric_value = false;
                        }
                        Some(_) => has_numeric_value = false,
                        None if has_numeric_value => {
                            value.value_or_nil =
                                Expr::new(value.loc, ExprData::Number(next_numeric_value));
                            let constant = crate::internal::js_ast::TsEnumValue {
                                number: next_numeric_value,
                                ..crate::internal::js_ast::TsEnumValue::default()
                            };
                            if value.reference != crate::internal::ast::INVALID_REF {
                                core.ts_enum_values_by_ref
                                    .insert(value.reference, constant.clone());
                            }
                            constants.insert(name, constant);
                            next_numeric_value += 1.0;
                        }
                        None => {
                            value.value_or_nil = Expr::new(value.loc, ExprData::Undefined);
                        }
                    }
                }
                core.should_fold_type_script_constant_expressions = old_should_fold;
                core.pop_scope();
                core.ts_enums.insert(enumeration.name.reference, constants);
            }
            Some(StmtData::Namespace(namespace)) => {
                core.record_declared_symbol(namespace.name.reference);
                core.record_declared_symbol(namespace.argument);
                core.push_scope_for_visit_pass(ScopeKind::Entry, statement.loc);
                visit_statements(core, &mut namespace.statements, resolve_identifiers);
                core.pop_scope();
            }
            Some(StmtData::ExportDefault(export)) => match export.value.data.as_deref_mut() {
                Some(StmtData::Expr(expression)) => {
                    visit_expr(core, &mut expression.value, resolve_identifiers);
                    keep_inferred_name(core, &mut expression.value, Some("default".into()));
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
                validate_single_statement(core, &if_statement.yes, SingleStatementContext::If);
                visit_statement(core, &mut if_statement.yes, resolve_identifiers);
                validate_single_statement(
                    core,
                    &if_statement.no_or_nil,
                    SingleStatementContext::If,
                );
                visit_statement(core, &mut if_statement.no_or_nil, resolve_identifiers);
            }
            Some(StmtData::DoWhile(loop_statement)) => {
                validate_single_statement(
                    core,
                    &loop_statement.body,
                    SingleStatementContext::Other,
                );
                core.visit_loop_depth += 1;
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                core.visit_loop_depth -= 1;
                visit_expr(core, &mut loop_statement.test, resolve_identifiers);
            }
            Some(StmtData::While(loop_statement)) => {
                visit_expr(core, &mut loop_statement.test, resolve_identifiers);
                validate_single_statement(
                    core,
                    &loop_statement.body,
                    SingleStatementContext::Other,
                );
                core.visit_loop_depth += 1;
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                core.visit_loop_depth -= 1;
            }
            Some(StmtData::With(with_statement)) => {
                if core.is_strict_mode() {
                    core.add_error_range(
                        crate::internal::js_lexer::range_of_identifier(&core.source, statement.loc),
                        "With statements cannot be used in strict mode",
                    );
                }
                visit_expr(core, &mut with_statement.value, resolve_identifiers);
                validate_single_statement(
                    core,
                    &with_statement.body,
                    SingleStatementContext::Other,
                );
                core.push_scope_for_visit_pass(ScopeKind::With, with_statement.body_loc);
                visit_statement(core, &mut with_statement.body, resolve_identifiers);
                core.pop_scope();
            }
            Some(StmtData::Throw(throw_statement)) => {
                visit_expr(core, &mut throw_statement.value, resolve_identifiers);
            }
            Some(StmtData::Return(return_statement)) => {
                if !core.is_inside_function_scope() && !core.is_inside_class_static_block() {
                    if core.is_file_considered_esm {
                        core.add_error_range(
                            crate::internal::js_lexer::range_of_identifier(
                                &core.source,
                                statement.loc,
                            ),
                            "Top-level return cannot be used inside an ECMAScript module",
                        );
                    } else {
                        core.has_top_level_return = true;
                    }
                }
                visit_expr(
                    core,
                    &mut return_statement.value_or_nil,
                    resolve_identifiers,
                );
            }
            Some(StmtData::For(loop_statement)) => {
                core.push_scope_for_visit_pass(ScopeKind::Block, statement.loc);
                visit_statement(core, &mut loop_statement.init_or_nil, resolve_identifiers);
                visit_expr(core, &mut loop_statement.test_or_nil, resolve_identifiers);
                visit_expr(core, &mut loop_statement.update_or_nil, resolve_identifiers);
                validate_single_statement(
                    core,
                    &loop_statement.body,
                    SingleStatementContext::Other,
                );
                core.visit_loop_depth += 1;
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                core.visit_loop_depth -= 1;
                core.pop_scope();
            }
            Some(StmtData::ForIn(loop_statement)) => {
                core.push_scope_for_visit_pass(ScopeKind::Block, statement.loc);
                visit_statement(core, &mut loop_statement.init, resolve_identifiers);
                visit_expr(core, &mut loop_statement.value, resolve_identifiers);
                validate_single_statement(
                    core,
                    &loop_statement.body,
                    SingleStatementContext::Other,
                );
                core.visit_loop_depth += 1;
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                core.visit_loop_depth -= 1;
                core.pop_scope();
            }
            Some(StmtData::ForOf(loop_statement)) => {
                core.push_scope_for_visit_pass(ScopeKind::Block, statement.loc);
                visit_statement(core, &mut loop_statement.init, resolve_identifiers);
                visit_expr(core, &mut loop_statement.value, resolve_identifiers);
                validate_single_statement(
                    core,
                    &loop_statement.body,
                    SingleStatementContext::Other,
                );
                core.visit_loop_depth += 1;
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                core.visit_loop_depth -= 1;
                core.pop_scope();
            }
            Some(StmtData::Label(label)) => {
                validate_single_statement(core, &label.statement, SingleStatementContext::Label);
                core.push_scope_for_visit_pass(ScopeKind::Label, statement.loc);
                let name = String::from_utf8_lossy(core.load_name_from_ref(label.name.reference))
                    .into_owned();
                let should_drop = core.options.drop_labels.contains(&name);
                let reference =
                    core.new_symbol(crate::internal::ast::SymbolKind::Label, name.clone());
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
                let old_control_flow_dead = core.is_control_flow_dead;
                if should_drop {
                    core.is_control_flow_dead = true;
                }
                visit_statement(core, &mut label.statement, resolve_identifiers);
                core.is_control_flow_dead = old_control_flow_dead;
                core.pop_scope();
                if should_drop {
                    statement.data = Some(Box::new(StmtData::Empty));
                } else if core.options.minify_syntax
                    && core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                        .use_count_estimate
                        == 0
                {
                    if label.statement.data.is_none() {
                        statement.data = None;
                        continue;
                    }
                    let mut replacements =
                        super::control_flow::append_if_or_label_body_preserving_scope(
                            Vec::new(),
                            std::mem::take(&mut label.statement),
                        );
                    if replacements.is_empty() {
                        statement.data = None;
                    } else if replacements.len() == 1 {
                        *statement = replacements.pop().expect("single replacement");
                    } else {
                        statement.data = Some(Box::new(StmtData::Block(BlockStmt {
                            statements: replacements,
                            ..BlockStmt::default()
                        })));
                    }
                }
            }
            Some(StmtData::Switch(switch)) => {
                visit_expr(core, &mut switch.test, resolve_identifiers);
                core.push_scope_for_visit_pass(ScopeKind::Block, switch.body_loc);
                core.visit_switch_depth += 1;
                for case in &mut switch.cases {
                    visit_expr(core, &mut case.value_or_nil, resolve_identifiers);
                    for statement in &case.body {
                        if matches!(
                            statement.data.as_deref(),
                            Some(StmtData::Local(local))
                                if matches!(
                                    local.kind,
                                    crate::internal::js_ast::LocalKind::Using
                                        | crate::internal::js_ast::LocalKind::AwaitUsing
                                )
                        ) {
                            core.add_error_range(
                                crate::internal::js_lexer::range_of_identifier(
                                    &core.source,
                                    statement.loc,
                                ),
                                "Cannot use a \"using\" declaration directly inside a switch case",
                            );
                        }
                    }
                    visit_statements(core, &mut case.body, resolve_identifiers);
                }
                core.visit_switch_depth -= 1;
                core.pop_scope();
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
                    if catch.binding_or_nil.data.is_some() {
                        record_binding(core, &mut catch.binding_or_nil);
                        visit_binding_initializers(
                            core,
                            &mut catch.binding_or_nil,
                            resolve_identifiers,
                        );
                    }
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
                if break_statement.label.is_none()
                    && core.visit_loop_depth == 0
                    && core.visit_switch_depth == 0
                {
                    core.add_error_range(
                        crate::internal::js_lexer::range_of_identifier(&core.source, statement.loc),
                        "Cannot use \"break\" here",
                    );
                }
                bind_label_reference(core, &mut break_statement.label, false);
            }
            Some(StmtData::Continue(continue_statement)) => {
                if continue_statement.label.is_none() && core.visit_loop_depth == 0 {
                    core.add_error_range(
                        crate::internal::js_lexer::range_of_identifier(&core.source, statement.loc),
                        "Cannot use \"continue\" here",
                    );
                }
                bind_label_reference(core, &mut continue_statement.label, true);
            }
            _ => {}
        }
        if core.options.minify_syntax {
            minify_constant_if_statement(statement);
            minify_control_flow_statement(statement);
            if let Some(StmtData::Expr(expression)) = statement.data.as_deref_mut() {
                let helpers = make_helper_context(|reference| {
                    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                        == SymbolKind::Unbound
                });
                expression.value = helpers
                    .simplify_unused_expr(&expression.value, core.options.unsupported_js_features);
                if expression.value.data.is_none() {
                    statement.data = None;
                }
            }
        }
        if core.options.drop_console
            && matches!(
                statement.data.as_deref(),
                Some(StmtData::Expr(expression))
                    if matches!(expression.value.data.as_deref(), Some(ExprData::Undefined))
            )
        {
            statement.data = None;
        }
        if core.options.minify_syntax
            && was_control_flow_dead
            && !super::dead_control_flow::should_keep_stmt_in_dead_control_flow(statement)
        {
            statement.data = None;
        }
        if core.options.minify_syntax
            && matches!(
                statement.data.as_deref(),
                Some(
                    StmtData::Return(_)
                        | StmtData::Throw(_)
                        | StmtData::Break(_)
                        | StmtData::Continue(_)
                )
            )
        {
            core.is_control_flow_dead = true;
        }
    }
    if core.options.minify_syntax {
        absorb_expressions_into_for_initializers(statements);
    }
    core.is_control_flow_dead = old_control_flow_dead;
}

fn statement_to_expr(statement: &Stmt) -> Option<Expr> {
    match statement.data.as_deref()? {
        StmtData::Expr(expression) => Some(expression.value.clone()),
        StmtData::Block(block)
            if block.statements.len() == 1
                && !super::control_flow::stmts_care_about_scope(&block.statements) =>
        {
            statement_to_expr(&block.statements[0])
        }
        _ => None,
    }
}

fn unwrap_single_statement_block(statement: Stmt) -> Stmt {
    let Some(StmtData::Block(block)) = statement.data.as_deref() else {
        return statement;
    };
    if block.statements.len() == 1
        && !super::control_flow::stmts_care_about_scope(&block.statements)
    {
        return block.statements[0].clone();
    }
    statement
}

fn minify_control_flow_statement(statement: &mut Stmt) {
    match statement.data.as_deref() {
        Some(StmtData::If(value)) => {
            let Some(yes) = statement_to_expr(&value.yes) else {
                return;
            };
            let expression = if value.no_or_nil.data.is_some() {
                let Some(no) = statement_to_expr(&value.no_or_nil) else {
                    return;
                };
                Expr::new(
                    statement.loc,
                    ExprData::If(IfExpr {
                        test: value.test.clone(),
                        yes,
                        no,
                    }),
                )
            } else {
                Expr::new(
                    statement.loc,
                    ExprData::Binary(BinaryExpr {
                        left: value.test.clone(),
                        right: yes,
                        op: OpCode::BinaryLogicalAnd,
                    }),
                )
            };
            statement.data = Some(Box::new(StmtData::Expr(ExprStmt {
                value: expression,
                ..ExprStmt::default()
            })));
        }
        Some(StmtData::While(value)) => {
            statement.data = Some(Box::new(StmtData::For(ForStmt {
                test_or_nil: value.test.clone(),
                body: unwrap_single_statement_block(value.body.clone()),
                is_single_line_body: value.is_single_line_body,
                ..ForStmt::default()
            })));
        }
        Some(StmtData::DoWhile(value)) => {
            let mut value = value.clone();
            value.body = unwrap_single_statement_block(value.body);
            statement.data = Some(Box::new(StmtData::DoWhile(value)));
        }
        _ => {}
    }
}

fn absorb_expressions_into_for_initializers(statements: &mut [Stmt]) {
    for index in 1..statements.len() {
        let (before, after) = statements.split_at_mut(index);
        let previous = &mut before[index - 1];
        let Some(StmtData::Expr(expression)) = previous.data.as_deref() else {
            continue;
        };
        let Some(StmtData::For(for_statement)) = after[0].data.as_deref_mut() else {
            continue;
        };
        if for_statement.init_or_nil.data.is_some() {
            continue;
        }
        for_statement.init_or_nil = Stmt::new(
            previous.loc,
            StmtData::Expr(ExprStmt {
                value: expression.value.clone(),
                ..ExprStmt::default()
            }),
        );
        previous.data = None;
    }
}

fn minify_constant_if_statement(statement: &mut Stmt) {
    let Some(StmtData::If(value)) = statement.data.as_deref() else {
        return;
    };
    let Some((is_truthy, crate::internal::js_ast::SideEffects::NoSideEffects)) =
        crate::internal::js_ast::to_boolean_with_side_effects(value.test.data.as_deref())
    else {
        return;
    };
    let (live, dead) = if is_truthy {
        (&value.yes, &value.no_or_nil)
    } else {
        (&value.no_or_nil, &value.yes)
    };
    let mut dead = dead.clone();
    if super::dead_control_flow::should_keep_stmt_in_dead_control_flow(&mut dead) {
        return;
    }
    if live.data.is_none() {
        statement.data = None;
        return;
    }
    let mut replacements =
        super::control_flow::append_if_or_label_body_preserving_scope(Vec::new(), live.clone());
    if replacements.len() == 1 {
        *statement = replacements.pop().expect("single replacement");
    } else {
        statement.data = Some(Box::new(StmtData::Block(BlockStmt {
            statements: replacements,
            ..BlockStmt::default()
        })));
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum SingleStatementContext {
    If,
    Label,
    Other,
}

fn validate_single_statement(
    core: &mut ParserCore,
    statement: &Stmt,
    context: SingleStatementContext,
) {
    match statement.data.as_deref() {
        Some(StmtData::Local(local)) if local.kind != crate::internal::js_ast::LocalKind::Var => {
            report_forbidden_single_statement(core, statement.loc);
        }
        Some(StmtData::Class(_)) => report_forbidden_single_statement(core, statement.loc),
        Some(StmtData::Function(function)) => {
            let is_annex_b_function =
                !function.function.is_async && !function.function.is_generator;
            if is_annex_b_function
                && matches!(
                    context,
                    SingleStatementContext::If | SingleStatementContext::Label
                )
            {
                if core.is_strict_mode() {
                    let place = if context == SingleStatementContext::If {
                        "if statements"
                    } else {
                        "labels"
                    };
                    let reason = if core.is_file_considered_esm {
                        "an ECMAScript module"
                    } else {
                        "strict mode"
                    };
                    core.add_error_range(
                        crate::internal::js_lexer::range_of_identifier(&core.source, statement.loc),
                        format!("Function declarations inside {place} cannot be used in {reason}"),
                    );
                }
            } else {
                report_forbidden_single_statement(core, statement.loc);
            }
        }
        Some(StmtData::Label(label)) => {
            validate_single_statement(
                core,
                &label.statement,
                if context == SingleStatementContext::Label {
                    SingleStatementContext::Label
                } else {
                    SingleStatementContext::Other
                },
            );
        }
        _ => {}
    }
}

fn report_forbidden_single_statement(core: &mut ParserCore, loc: Loc) {
    core.add_error_range(
        crate::internal::js_lexer::range_of_identifier(&core.source, loc),
        "Cannot use a declaration in a single-statement context",
    );
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
    let old_loop_depth = std::mem::take(&mut core.visit_loop_depth);
    let old_switch_depth = std::mem::take(&mut core.visit_switch_depth);
    let old_new_target_allowed = std::mem::replace(&mut core.visit_new_target_allowed, true);
    if let Some(name) = function.name
        && !ParserCore::is_stored_name_ref(name.reference)
    {
        core.record_declared_symbol(name.reference);
    }
    core.push_scope_for_visit_pass(ScopeKind::FunctionArgs, function.open_paren_loc);
    let use_strict_loc = function_body_use_strict(core, &function.body.block.statements);
    if use_strict_loc.is_some() {
        crate::internal::js_ast::Scope::recursive_set_strict_mode(
            core.current_scope
                .as_ref()
                .expect("function arguments scope"),
            StrictModeKind::ExplicitStrict,
        );
    }
    if let Some(name) = function.name {
        validate_binding_name(core, name.loc, name.reference);
    }
    let has_simple_args = is_simple_parameter_list(&function.args, function.has_rest_arg);
    if let Some(loc) = use_strict_loc
        && !has_simple_args
    {
        core.add_error_range(
            core.source.range_of_string(loc),
            "Cannot use a \"use strict\" directive in a function with a non-simple parameter list",
        );
    }
    let check_duplicates =
        function.is_unique_formal_parameters || !has_simple_args || core.is_strict_mode();
    let mut duplicate_args = HashMap::new();
    for argument in &mut function.args {
        for decorator in &mut argument.decorators {
            visit_expr(core, &mut decorator.value, resolve_identifiers);
        }
        record_binding_with_duplicates(
            core,
            &mut argument.binding,
            check_duplicates.then_some(&mut duplicate_args),
        );
        visit_binding_initializers(core, &mut argument.binding, resolve_identifiers);
        visit_expr(core, &mut argument.default_or_nil, resolve_identifiers);
        let name = inferred_name_from_binding(core, &argument.binding);
        keep_inferred_name(core, &mut argument.default_or_nil, name);
    }
    core.push_scope_for_visit_pass(ScopeKind::FunctionBody, function.body.loc);
    visit_statements(
        core,
        &mut function.body.block.statements,
        resolve_identifiers,
    );
    core.pop_scope();
    core.pop_scope();
    core.visit_loop_depth = old_loop_depth;
    core.visit_switch_depth = old_switch_depth;
    core.visit_new_target_allowed = old_new_target_allowed;
}

#[allow(clippy::too_many_lines)]
fn visit_class(core: &mut ParserCore, class: &mut Class, resolve_identifiers: bool) {
    lower_type_script_constructor_parameter_fields(core, class);
    let outer_class_name = class.name.and_then(|name| {
        (!ParserCore::is_stored_name_ref(name.reference)).then_some(name.reference)
    });
    if let Some(reference) = outer_class_name {
        core.record_declared_symbol(reference);
    }
    for decorator in &mut class.decorators {
        visit_expr(core, &mut decorator.value, resolve_identifiers);
    }
    core.push_scope_for_visit_pass(ScopeKind::ClassName, class.class_keyword.loc);
    let mut inner_class_name = None;
    if let Some(name) = &mut class.name {
        let text = if ParserCore::is_stored_name_ref(name.reference) {
            String::from_utf8_lossy(core.load_name_from_ref(name.reference)).into_owned()
        } else {
            core.symbols[usize::try_from(name.reference.inner_index).expect("symbol index")]
                .original_name
                .clone()
        };
        let inner_reference =
            core.new_symbol(crate::internal::ast::SymbolKind::Const, text.clone());
        inner_class_name = Some(inner_reference);
        core.record_declared_symbol(inner_reference);
        core.current_scope
            .as_ref()
            .expect("class name scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .insert(
                text,
                crate::internal::js_ast::ScopeMember {
                    reference: inner_reference,
                    loc: name.loc,
                },
            );
        if ParserCore::is_stored_name_ref(name.reference) {
            name.reference = inner_reference;
        }
    }
    crate::internal::js_ast::Scope::recursive_set_strict_mode(
        core.current_scope.as_ref().expect("class name scope"),
        crate::internal::js_ast::StrictModeKind::ImplicitStrictClass,
    );
    if let Some(name) = class.name {
        validate_binding_name(core, name.loc, name.reference);
    }
    visit_expr(core, &mut class.extends_or_nil, resolve_identifiers);
    core.push_scope_for_visit_pass(ScopeKind::ClassBody, class.body_loc);
    report_duplicate_properties(core, &class.properties, DuplicatePropertiesIn::Class);
    if core.options.ts.parse && !class.use_define_for_class_fields {
        class.properties.retain(|property| {
            property.kind != PropertyKind::Field
                || property.initializer_or_nil.data.is_some()
                || property.value_or_nil.data.is_some()
                || !property.decorators.is_empty()
                || property.flags.contains(PropertyFlags::IS_COMPUTED)
                || matches!(
                    property.key.data.as_deref(),
                    Some(ExprData::PrivateIdentifier(_))
                )
        });
    }
    for property in &mut class.properties {
        if let Some(ExprData::PrivateIdentifier(private)) = property.key.data.as_deref_mut() {
            core.record_declared_symbol(private.reference);
            let symbol_index =
                usize::try_from(private.reference.inner_index).expect("symbol index");
            let name = core.symbols[symbol_index].original_name.clone();
            if core
                .lower_all_of_these_private_names
                .get(&name)
                .copied()
                .unwrap_or(false)
            {
                core.symbols[symbol_index].flags |=
                    crate::internal::ast::SymbolFlags::PRIVATE_SYMBOL_MUST_BE_LOWERED;
            }
        }
        if property.flags.contains(PropertyFlags::IS_COMPUTED) {
            visit_expr(core, &mut property.key, resolve_identifiers);
        }
        for decorator in &mut property.decorators {
            visit_expr(core, &mut decorator.value, resolve_identifiers);
        }
        core.current_scope
            .as_ref()
            .expect("class body scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forbid_arguments = true;
        let old_new_target_allowed = std::mem::replace(&mut core.visit_new_target_allowed, true);
        visit_expr(core, &mut property.value_or_nil, resolve_identifiers);
        visit_expr(core, &mut property.initializer_or_nil, resolve_identifiers);
        if matches!(
            property.kind,
            PropertyKind::Field | PropertyKind::AutoAccessor
        ) {
            let name = inferred_name_from_expression(core, &property.key);
            keep_inferred_name(core, &mut property.initializer_or_nil, name);
        }
        core.visit_new_target_allowed = old_new_target_allowed;
        core.current_scope
            .as_ref()
            .expect("class body scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forbid_arguments = false;
        if let Some(static_block) = &mut property.class_static_block {
            let old_loop_depth = std::mem::take(&mut core.visit_loop_depth);
            let old_switch_depth = std::mem::take(&mut core.visit_switch_depth);
            let old_new_target_allowed =
                std::mem::replace(&mut core.visit_new_target_allowed, true);
            core.push_scope_for_visit_pass(ScopeKind::ClassStaticInit, static_block.loc);
            core.current_scope
                .as_ref()
                .expect("class static block scope")
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forbid_arguments = true;
            visit_statements(
                core,
                &mut static_block.block.statements,
                resolve_identifiers,
            );
            core.pop_scope();
            core.visit_loop_depth = old_loop_depth;
            core.visit_switch_depth = old_switch_depth;
            core.visit_new_target_allowed = old_new_target_allowed;
        }
    }
    lower_type_script_static_field_assignments(class);
    lower_type_script_class_field_assignments(core, class);
    core.pop_scope();
    core.pop_scope();
    if let (Some(inner), Some(outer)) = (inner_class_name, outer_class_name) {
        core.merge_symbols(inner, outer);
    }
}

fn lower_type_script_class_field_assignments(core: &mut ParserCore, class: &mut Class) {
    if class.use_define_for_class_fields {
        return;
    }
    let is_derived = class.extends_or_nil.data.is_some();
    let has_constructor = class_constructor_index(class).is_some();
    let allow_any_initializer = !has_constructor || class_constructor_is_binding_free(class);
    let (mut constructor_binding_names, constructor_binding_refs) =
        class_constructor_bindings(core, class);
    if class.properties.iter().any(|property| {
        class_field_has_binding_collision(core, property, &constructor_binding_names)
    }) {
        rename_constructor_bindings(
            core,
            &constructor_binding_refs,
            &mut constructor_binding_names,
        );
    }
    let lower_private_fields = class.properties.iter().any(|property| {
        !matches!(
            property.key.data.as_deref(),
            Some(ExprData::PrivateIdentifier(_))
        ) && class_field_can_be_moved(
            core,
            property,
            allow_any_initializer,
            &constructor_binding_names,
        )
    });

    let mut assignments = Vec::new();
    let mut private_declarations = Vec::new();
    class.properties.retain_mut(|property| {
        let Some((assignment, keep_private_declaration)) = take_class_field_assignment(
            core,
            property,
            lower_private_fields,
            allow_any_initializer,
            &constructor_binding_names,
        ) else {
            return true;
        };
        assignments.push(assignment);
        if keep_private_declaration {
            private_declarations.push(std::mem::take(property));
        }
        false
    });
    if assignments.is_empty() {
        return;
    }

    if let Some(constructor_index) = class_constructor_index(class) {
        let constructor = &mut class.properties[constructor_index];
        let Some(ExprData::Function(function)) = constructor.value_or_nil.data.as_deref_mut()
        else {
            return;
        };
        if is_derived {
            insert_parameter_fields_after_super(
                &mut function.function.body.block.statements,
                &assignments,
            );
        } else {
            function
                .function
                .body
                .block
                .statements
                .splice(0..0, assignments);
        }
    } else {
        append_class_field_constructor(core, class, assignments, is_derived);
    }
    class.properties.extend(private_declarations);
}

fn append_class_field_constructor(
    core: &mut ParserCore,
    class: &mut Class,
    assignments: Vec<Stmt>,
    is_derived: bool,
) {
    let loc = class.body_loc;
    let mut function = Function::default();
    function.body.loc = loc;
    function.body.block.statements = assignments;
    if is_derived {
        let arguments_ref = core.new_symbol(crate::internal::ast::SymbolKind::Unbound, "arguments");
        core.record_usage(arguments_ref);
        function.body.block.statements.insert(
            0,
            Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value: Expr::new(
                        loc,
                        ExprData::Call(CallExpr {
                            target: Expr::new(loc, ExprData::Super),
                            args: vec![Expr::new(
                                loc,
                                ExprData::Spread(crate::internal::js_ast::SpreadExpr {
                                    value: Expr::new(
                                        loc,
                                        ExprData::Identifier(IdentifierExpr {
                                            reference: arguments_ref,
                                            ..IdentifierExpr::default()
                                        }),
                                    ),
                                }),
                            )],
                            ..CallExpr::default()
                        }),
                    ),
                    ..ExprStmt::default()
                }),
            ),
        );
    }
    class.properties.push(Property {
        key: Expr::new(
            loc,
            ExprData::String(StringExpr {
                value: string_to_utf16(b"constructor"),
                ..StringExpr::default()
            }),
        ),
        value_or_nil: Expr::new(
            loc,
            ExprData::Function(FunctionExpr {
                function,
                ..FunctionExpr::default()
            }),
        ),
        loc,
        kind: PropertyKind::Method,
        ..Property::default()
    });
}

fn class_constructor_index(class: &Class) -> Option<usize> {
    class.properties.iter().position(|property| {
        property.kind == PropertyKind::Method
            && matches!(
                property.key.data.as_deref(),
                Some(ExprData::String(name))
                    if utf16_to_string(&name.value) == b"constructor"
            )
    })
}

fn class_constructor_is_binding_free(class: &Class) -> bool {
    let Some(index) = class_constructor_index(class) else {
        return true;
    };
    let Some(ExprData::Function(function)) = class.properties[index].value_or_nil.data.as_deref()
    else {
        return false;
    };
    function.function.args.is_empty()
        && !function
            .function
            .body
            .block
            .statements
            .iter()
            .any(statement_contains_binding)
}

fn class_constructor_bindings(
    core: &ParserCore,
    class: &Class,
) -> (HashSet<String>, Vec<crate::internal::ast::Ref>) {
    let mut names = HashSet::new();
    let mut references = Vec::new();
    let Some(index) = class_constructor_index(class) else {
        return (names, references);
    };
    let Some(ExprData::Function(function)) = class.properties[index].value_or_nil.data.as_deref()
    else {
        return (names, references);
    };
    for argument in &function.function.args {
        collect_bindings(core, &argument.binding, &mut names, &mut references);
    }
    for statement in &function.function.body.block.statements {
        collect_statement_bindings(core, statement, &mut names, &mut references);
    }
    (names, references)
}

fn collect_bindings(
    core: &ParserCore,
    binding: &Binding,
    names: &mut HashSet<String>,
    references: &mut Vec<crate::internal::ast::Ref>,
) {
    match binding.data.as_deref() {
        Some(BindingData::Identifier(identifier)) => {
            names.insert(symbol_name(core, identifier.reference));
            references.push(identifier.reference);
        }
        Some(BindingData::Array(array)) => {
            for item in &array.items {
                collect_bindings(core, &item.binding, names, references);
            }
        }
        Some(BindingData::Object(object)) => {
            for property in &object.properties {
                collect_bindings(core, &property.value, names, references);
            }
        }
        Some(BindingData::Missing) | None => {}
    }
}

fn collect_statement_bindings(
    core: &ParserCore,
    statement: &Stmt,
    names: &mut HashSet<String>,
    references: &mut Vec<crate::internal::ast::Ref>,
) {
    match statement.data.as_deref() {
        Some(StmtData::Local(local)) => {
            for declaration in &local.declarations {
                collect_bindings(core, &declaration.binding, names, references);
            }
        }
        Some(StmtData::Function(function)) => {
            if let Some(name) = function.function.name {
                names.insert(symbol_name(core, name.reference));
                references.push(name.reference);
            }
        }
        Some(StmtData::Class(class)) => {
            if let Some(name) = class.class.name {
                names.insert(symbol_name(core, name.reference));
                references.push(name.reference);
            }
        }
        Some(StmtData::Block(value)) => {
            for statement in &value.statements {
                collect_statement_bindings(core, statement, names, references);
            }
        }
        Some(StmtData::If(value)) => {
            collect_statement_bindings(core, &value.yes, names, references);
            collect_statement_bindings(core, &value.no_or_nil, names, references);
        }
        Some(StmtData::For(value)) => {
            collect_statement_bindings(core, &value.init_or_nil, names, references);
            collect_statement_bindings(core, &value.body, names, references);
        }
        Some(StmtData::ForIn(value)) => {
            collect_statement_bindings(core, &value.init, names, references);
            collect_statement_bindings(core, &value.body, names, references);
        }
        Some(StmtData::ForOf(value)) => {
            collect_statement_bindings(core, &value.init, names, references);
            collect_statement_bindings(core, &value.body, names, references);
        }
        Some(StmtData::DoWhile(value)) => {
            collect_statement_bindings(core, &value.body, names, references);
        }
        Some(StmtData::While(value)) => {
            collect_statement_bindings(core, &value.body, names, references);
        }
        Some(StmtData::With(value)) => {
            collect_statement_bindings(core, &value.body, names, references);
        }
        Some(StmtData::Label(value)) => {
            collect_statement_bindings(core, &value.statement, names, references);
        }
        Some(StmtData::Try(value)) => {
            for statement in &value.block.statements {
                collect_statement_bindings(core, statement, names, references);
            }
            if let Some(catch) = &value.catch {
                collect_bindings(core, &catch.binding_or_nil, names, references);
                for statement in &catch.block.statements {
                    collect_statement_bindings(core, statement, names, references);
                }
            }
            if let Some(finally) = &value.finally {
                for statement in &finally.block.statements {
                    collect_statement_bindings(core, statement, names, references);
                }
            }
        }
        Some(StmtData::Switch(value)) => {
            for case in &value.cases {
                for statement in &case.body {
                    collect_statement_bindings(core, statement, names, references);
                }
            }
        }
        _ => {}
    }
}

fn class_field_has_binding_collision(
    core: &ParserCore,
    property: &Property,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    if property.kind != PropertyKind::Field
        || property.flags.contains(PropertyFlags::IS_STATIC)
        || !property.decorators.is_empty()
        || class_field_assignment_target(property).is_none()
    {
        return false;
    }
    let initializer = if property.initializer_or_nil.data.is_some() {
        &property.initializer_or_nil
    } else {
        &property.value_or_nil
    };
    initializer.data.is_some()
        && class_field_initializer_has_binding_collision(
            core,
            initializer,
            constructor_binding_names,
        )
}

fn rename_constructor_bindings(
    core: &mut ParserCore,
    references: &[crate::internal::ast::Ref],
    constructor_binding_names: &mut HashSet<String>,
) {
    let mut used_names = core
        .symbols
        .iter()
        .map(|symbol| symbol.original_name.clone())
        .collect::<HashSet<_>>();
    let mut renamed_references = HashSet::new();
    constructor_binding_names.clear();
    for &reference in references {
        if !renamed_references.insert(reference) {
            continue;
        }
        let symbol_index = usize::try_from(reference.inner_index).expect("symbol index");
        let original_name = core.symbols[symbol_index].original_name.clone();
        let mut suffix = 2_u32;
        let replacement = loop {
            let candidate = format!("{original_name}{suffix}");
            if used_names.insert(candidate.clone()) {
                break candidate;
            }
            suffix += 1;
        };
        core.symbols[symbol_index]
            .original_name
            .clone_from(&replacement);
        constructor_binding_names.insert(replacement);
    }
}

fn statement_contains_binding(statement: &Stmt) -> bool {
    match statement.data.as_deref() {
        Some(
            StmtData::Local(_) | StmtData::Function(_) | StmtData::Class(_) | StmtData::Try(_),
        ) => true,
        Some(StmtData::Block(value)) => value.statements.iter().any(statement_contains_binding),
        Some(StmtData::If(value)) => {
            statement_contains_binding(&value.yes) || statement_contains_binding(&value.no_or_nil)
        }
        Some(StmtData::For(value)) => {
            statement_contains_binding(&value.init_or_nil)
                || statement_contains_binding(&value.body)
        }
        Some(StmtData::ForIn(value)) => {
            statement_contains_binding(&value.init) || statement_contains_binding(&value.body)
        }
        Some(StmtData::ForOf(value)) => {
            statement_contains_binding(&value.init) || statement_contains_binding(&value.body)
        }
        Some(StmtData::DoWhile(value)) => statement_contains_binding(&value.body),
        Some(StmtData::While(value)) => statement_contains_binding(&value.body),
        Some(StmtData::With(value)) => statement_contains_binding(&value.body),
        Some(StmtData::Label(value)) => statement_contains_binding(&value.statement),
        Some(StmtData::Switch(value)) => value
            .cases
            .iter()
            .any(|case| case.body.iter().any(statement_contains_binding)),
        _ => false,
    }
}

fn class_field_can_be_moved(
    core: &ParserCore,
    property: &Property,
    allow_any_initializer: bool,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    if property.kind != PropertyKind::Field
        || property.flags.contains(PropertyFlags::IS_STATIC)
        || !property.decorators.is_empty()
    {
        return false;
    }
    let initializer = if property.initializer_or_nil.data.is_some() {
        &property.initializer_or_nil
    } else {
        &property.value_or_nil
    };
    initializer.data.is_some()
        && (allow_any_initializer
            || class_field_initializer_is_safe_to_move(initializer)
            || !class_field_initializer_has_binding_collision(
                core,
                initializer,
                constructor_binding_names,
            ))
        && class_field_assignment_target(property).is_some()
}

fn take_class_field_assignment(
    core: &ParserCore,
    property: &mut Property,
    lower_private_fields: bool,
    allow_any_initializer: bool,
    constructor_binding_names: &HashSet<String>,
) -> Option<(Stmt, bool)> {
    let is_private = matches!(
        property.key.data.as_deref(),
        Some(ExprData::PrivateIdentifier(_))
    );
    if is_private && !lower_private_fields {
        return None;
    }
    if !class_field_can_be_moved(
        core,
        property,
        allow_any_initializer,
        constructor_binding_names,
    ) {
        return None;
    }
    let target = class_field_assignment_target(property)?;
    let initializer = if property.initializer_or_nil.data.is_some() {
        std::mem::take(&mut property.initializer_or_nil)
    } else {
        std::mem::take(&mut property.value_or_nil)
    };
    Some((
        class_field_assignment(property.loc, target, initializer),
        is_private,
    ))
}

fn class_field_assignment_target(property: &Property) -> Option<Expr> {
    match property.key.data.as_deref()? {
        ExprData::String(key)
            if !property.flags.contains(PropertyFlags::IS_COMPUTED)
                && !property.flags.contains(PropertyFlags::PREFER_QUOTED_KEY)
                && is_identifier_es5_and_es_next(&String::from_utf16_lossy(&key.value)) =>
        {
            Expr::new(
                property.loc,
                ExprData::Dot(DotExpr {
                    target: Expr::new(property.loc, ExprData::This),
                    name: String::from_utf16_lossy(&key.value),
                    name_loc: property.key.loc,
                    ..DotExpr::default()
                }),
            )
        }
        ExprData::String(_) | ExprData::Number(_) | ExprData::PrivateIdentifier(_) => Expr::new(
            property.loc,
            ExprData::Index(crate::internal::js_ast::IndexExpr {
                target: Expr::new(property.loc, ExprData::This),
                index: property.key.clone(),
                ..crate::internal::js_ast::IndexExpr::default()
            }),
        ),
        _ => return None,
    }
    .into()
}

fn class_field_assignment(loc: Loc, target: Expr, initializer: Expr) -> Stmt {
    Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: Expr::new(
                loc,
                ExprData::Binary(BinaryExpr {
                    left: target,
                    right: initializer,
                    op: OpCode::BinaryAssign,
                }),
            ),
            ..ExprStmt::default()
        }),
    )
}

fn lower_type_script_static_field_assignments(class: &mut Class) {
    if class.use_define_for_class_fields {
        return;
    }
    for property in &mut class.properties {
        if property.kind != PropertyKind::Field
            || !property.flags.contains(PropertyFlags::IS_STATIC)
            || !property.decorators.is_empty()
            || matches!(
                property.key.data.as_deref(),
                Some(ExprData::PrivateIdentifier(_))
            )
        {
            continue;
        }
        let Some(target) = class_field_assignment_target(property) else {
            continue;
        };
        let initializer = if property.initializer_or_nil.data.is_some() {
            std::mem::take(&mut property.initializer_or_nil)
        } else if property.value_or_nil.data.is_some() {
            std::mem::take(&mut property.value_or_nil)
        } else {
            continue;
        };
        let loc = property.loc;
        *property = Property {
            class_static_block: Some(Box::new(ClassStaticBlock {
                block: BlockStmt {
                    statements: vec![class_field_assignment(loc, target, initializer)],
                    ..BlockStmt::default()
                },
                loc,
            })),
            loc,
            kind: PropertyKind::ClassStaticBlock,
            ..Property::default()
        };
    }
}

fn class_field_initializer_is_safe_to_move(expression: &Expr) -> bool {
    match expression.data.as_deref() {
        Some(
            ExprData::Boolean(_)
            | ExprData::Null
            | ExprData::Undefined
            | ExprData::This
            | ExprData::NewTarget(_)
            | ExprData::ImportMeta(_)
            | ExprData::JsxText(_)
            | ExprData::Missing
            | ExprData::Number(_)
            | ExprData::BigInt(_)
            | ExprData::String(_)
            | ExprData::RegExp(_)
            | ExprData::RequireString(_)
            | ExprData::RequireResolveString(_)
            | ExprData::ImportString(_),
        ) => true,
        Some(ExprData::Array(value)) => value
            .items
            .iter()
            .all(|item| item.data.is_none() || class_field_initializer_is_safe_to_move(item)),
        Some(ExprData::Unary(value)) => class_field_initializer_is_safe_to_move(&value.value),
        Some(ExprData::Binary(value)) => {
            class_field_initializer_is_safe_to_move(&value.left)
                && class_field_initializer_is_safe_to_move(&value.right)
        }
        Some(ExprData::New(value)) => {
            class_field_initializer_is_safe_to_move(&value.target)
                && value
                    .args
                    .iter()
                    .all(class_field_initializer_is_safe_to_move)
        }
        Some(ExprData::Call(value)) => {
            class_field_initializer_is_safe_to_move(&value.target)
                && value
                    .args
                    .iter()
                    .all(class_field_initializer_is_safe_to_move)
        }
        Some(ExprData::Dot(value)) => class_field_initializer_is_safe_to_move(&value.target),
        Some(ExprData::Index(value)) => {
            class_field_initializer_is_safe_to_move(&value.target)
                && class_field_initializer_is_safe_to_move(&value.index)
        }
        Some(ExprData::Object(value)) => value.properties.iter().all(|property| {
            (!property.flags.contains(PropertyFlags::IS_COMPUTED)
                || class_field_initializer_is_safe_to_move(&property.key))
                && class_field_initializer_is_safe_to_move(&property.value_or_nil)
                && class_field_initializer_is_safe_to_move(&property.initializer_or_nil)
                && property.decorators.is_empty()
        }),
        Some(ExprData::Spread(value)) => class_field_initializer_is_safe_to_move(&value.value),
        Some(ExprData::Template(value)) => {
            (value.tag_or_nil.data.is_none()
                || class_field_initializer_is_safe_to_move(&value.tag_or_nil))
                && value
                    .parts
                    .iter()
                    .all(|part| class_field_initializer_is_safe_to_move(&part.value))
        }
        Some(ExprData::InlinedEnum(value)) => class_field_initializer_is_safe_to_move(&value.value),
        Some(ExprData::Annotation(value)) => class_field_initializer_is_safe_to_move(&value.value),
        Some(ExprData::Await(value)) => class_field_initializer_is_safe_to_move(&value.value),
        Some(ExprData::Yield(value)) => {
            value.value_or_nil.data.is_none()
                || class_field_initializer_is_safe_to_move(&value.value_or_nil)
        }
        Some(ExprData::If(value)) => {
            class_field_initializer_is_safe_to_move(&value.test)
                && class_field_initializer_is_safe_to_move(&value.yes)
                && class_field_initializer_is_safe_to_move(&value.no)
        }
        Some(ExprData::ImportCall(value)) => {
            class_field_initializer_is_safe_to_move(&value.expr)
                && (value.options_or_nil.data.is_none()
                    || class_field_initializer_is_safe_to_move(&value.options_or_nil))
        }
        Some(
            ExprData::Super
            | ExprData::Arrow(_)
            | ExprData::Function(_)
            | ExprData::Class(_)
            | ExprData::Identifier(_)
            | ExprData::ImportIdentifier(_)
            | ExprData::PrivateIdentifier(_)
            | ExprData::NameOfSymbol(_)
            | ExprData::JsxElement(_),
        )
        | None => false,
    }
}

#[allow(clippy::too_many_lines)]
fn class_field_initializer_has_binding_collision(
    core: &ParserCore,
    expression: &Expr,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    let collides = |reference| constructor_binding_names.contains(&symbol_name(core, reference));
    match expression.data.as_deref() {
        Some(ExprData::Identifier(value)) => collides(value.reference),
        Some(ExprData::ImportIdentifier(value)) => collides(value.reference),
        Some(ExprData::NameOfSymbol(value)) => collides(value.reference),
        Some(ExprData::Array(value)) => value.items.iter().any(|item| {
            class_field_initializer_has_binding_collision(core, item, constructor_binding_names)
        }),
        Some(ExprData::Unary(value)) => class_field_initializer_has_binding_collision(
            core,
            &value.value,
            constructor_binding_names,
        ),
        Some(ExprData::Binary(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.left,
                constructor_binding_names,
            ) || class_field_initializer_has_binding_collision(
                core,
                &value.right,
                constructor_binding_names,
            )
        }
        Some(ExprData::New(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.target,
                constructor_binding_names,
            ) || value.args.iter().any(|argument| {
                class_field_initializer_has_binding_collision(
                    core,
                    argument,
                    constructor_binding_names,
                )
            })
        }
        Some(ExprData::Call(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.target,
                constructor_binding_names,
            ) || value.args.iter().any(|argument| {
                class_field_initializer_has_binding_collision(
                    core,
                    argument,
                    constructor_binding_names,
                )
            })
        }
        Some(ExprData::Dot(value)) => class_field_initializer_has_binding_collision(
            core,
            &value.target,
            constructor_binding_names,
        ),
        Some(ExprData::Index(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.target,
                constructor_binding_names,
            ) || class_field_initializer_has_binding_collision(
                core,
                &value.index,
                constructor_binding_names,
            )
        }
        Some(ExprData::Object(value)) => value.properties.iter().any(|property| {
            class_field_initializer_has_binding_collision(
                core,
                &property.key,
                constructor_binding_names,
            ) || class_field_initializer_has_binding_collision(
                core,
                &property.value_or_nil,
                constructor_binding_names,
            ) || class_field_initializer_has_binding_collision(
                core,
                &property.initializer_or_nil,
                constructor_binding_names,
            ) || property.decorators.iter().any(|decorator| {
                class_field_initializer_has_binding_collision(
                    core,
                    &decorator.value,
                    constructor_binding_names,
                )
            })
        }),
        Some(ExprData::Spread(value)) => class_field_initializer_has_binding_collision(
            core,
            &value.value,
            constructor_binding_names,
        ),
        Some(ExprData::Template(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.tag_or_nil,
                constructor_binding_names,
            ) || value.parts.iter().any(|part| {
                class_field_initializer_has_binding_collision(
                    core,
                    &part.value,
                    constructor_binding_names,
                )
            })
        }
        Some(ExprData::InlinedEnum(value)) => class_field_initializer_has_binding_collision(
            core,
            &value.value,
            constructor_binding_names,
        ),
        Some(ExprData::Annotation(value)) => class_field_initializer_has_binding_collision(
            core,
            &value.value,
            constructor_binding_names,
        ),
        Some(ExprData::Await(value)) => class_field_initializer_has_binding_collision(
            core,
            &value.value,
            constructor_binding_names,
        ),
        Some(ExprData::Yield(value)) => class_field_initializer_has_binding_collision(
            core,
            &value.value_or_nil,
            constructor_binding_names,
        ),
        Some(ExprData::If(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.test,
                constructor_binding_names,
            ) || class_field_initializer_has_binding_collision(
                core,
                &value.yes,
                constructor_binding_names,
            ) || class_field_initializer_has_binding_collision(
                core,
                &value.no,
                constructor_binding_names,
            )
        }
        Some(ExprData::ImportCall(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.expr,
                constructor_binding_names,
            ) || class_field_initializer_has_binding_collision(
                core,
                &value.options_or_nil,
                constructor_binding_names,
            )
        }
        Some(ExprData::Arrow(value)) => {
            arguments_have_binding_collision(core, &value.args, constructor_binding_names)
                || statements_have_binding_collision(
                    core,
                    &value.body.block.statements,
                    constructor_binding_names,
                )
        }
        Some(ExprData::Function(value)) => {
            function_has_binding_collision(core, &value.function, constructor_binding_names)
        }
        Some(ExprData::Class(value)) => {
            class_has_binding_collision(core, &value.class, constructor_binding_names)
        }
        Some(ExprData::JsxElement(value)) => {
            class_field_initializer_has_binding_collision(
                core,
                &value.tag_or_nil,
                constructor_binding_names,
            ) || value.properties.iter().any(|property| {
                property_has_binding_collision(core, property, constructor_binding_names)
            }) || value.nullable_children.iter().any(|child| {
                class_field_initializer_has_binding_collision(
                    core,
                    child,
                    constructor_binding_names,
                )
            })
        }
        Some(
            ExprData::Boolean(_)
            | ExprData::Super
            | ExprData::Null
            | ExprData::Undefined
            | ExprData::This
            | ExprData::NewTarget(_)
            | ExprData::ImportMeta(_)
            | ExprData::PrivateIdentifier(_)
            | ExprData::JsxText(_)
            | ExprData::Missing
            | ExprData::Number(_)
            | ExprData::BigInt(_)
            | ExprData::String(_)
            | ExprData::RegExp(_)
            | ExprData::RequireString(_)
            | ExprData::RequireResolveString(_)
            | ExprData::ImportString(_),
        )
        | None => false,
    }
}

fn binding_has_binding_collision(
    core: &ParserCore,
    binding: &Binding,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    match binding.data.as_deref() {
        Some(BindingData::Array(value)) => value.items.iter().any(|item| {
            binding_has_binding_collision(core, &item.binding, constructor_binding_names)
                || class_field_initializer_has_binding_collision(
                    core,
                    &item.default_value_or_nil,
                    constructor_binding_names,
                )
        }),
        Some(BindingData::Object(value)) => value.properties.iter().any(|property| {
            (property.is_computed
                && class_field_initializer_has_binding_collision(
                    core,
                    &property.key,
                    constructor_binding_names,
                ))
                || binding_has_binding_collision(core, &property.value, constructor_binding_names)
                || class_field_initializer_has_binding_collision(
                    core,
                    &property.default_value_or_nil,
                    constructor_binding_names,
                )
        }),
        Some(BindingData::Missing | BindingData::Identifier(_)) | None => false,
    }
}

fn arguments_have_binding_collision(
    core: &ParserCore,
    arguments: &[crate::internal::js_ast::Arg],
    constructor_binding_names: &HashSet<String>,
) -> bool {
    arguments.iter().any(|argument| {
        binding_has_binding_collision(core, &argument.binding, constructor_binding_names)
            || class_field_initializer_has_binding_collision(
                core,
                &argument.default_or_nil,
                constructor_binding_names,
            )
            || argument.decorators.iter().any(|decorator| {
                class_field_initializer_has_binding_collision(
                    core,
                    &decorator.value,
                    constructor_binding_names,
                )
            })
    })
}

fn function_has_binding_collision(
    core: &ParserCore,
    function: &Function,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    arguments_have_binding_collision(core, &function.args, constructor_binding_names)
        || statements_have_binding_collision(
            core,
            &function.body.block.statements,
            constructor_binding_names,
        )
}

fn property_has_binding_collision(
    core: &ParserCore,
    property: &Property,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    class_field_initializer_has_binding_collision(core, &property.key, constructor_binding_names)
        || class_field_initializer_has_binding_collision(
            core,
            &property.value_or_nil,
            constructor_binding_names,
        )
        || class_field_initializer_has_binding_collision(
            core,
            &property.initializer_or_nil,
            constructor_binding_names,
        )
        || property.decorators.iter().any(|decorator| {
            class_field_initializer_has_binding_collision(
                core,
                &decorator.value,
                constructor_binding_names,
            )
        })
        || property.class_static_block.as_ref().is_some_and(|block| {
            statements_have_binding_collision(
                core,
                &block.block.statements,
                constructor_binding_names,
            )
        })
}

fn class_has_binding_collision(
    core: &ParserCore,
    class: &Class,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    class.decorators.iter().any(|decorator| {
        class_field_initializer_has_binding_collision(
            core,
            &decorator.value,
            constructor_binding_names,
        )
    }) || class_field_initializer_has_binding_collision(
        core,
        &class.extends_or_nil,
        constructor_binding_names,
    ) || class
        .properties
        .iter()
        .any(|property| property_has_binding_collision(core, property, constructor_binding_names))
}

fn statements_have_binding_collision(
    core: &ParserCore,
    statements: &[Stmt],
    constructor_binding_names: &HashSet<String>,
) -> bool {
    statements.iter().any(|statement| {
        statement_has_binding_collision(core, statement, constructor_binding_names)
    })
}

#[allow(clippy::too_many_lines)]
fn statement_has_binding_collision(
    core: &ParserCore,
    statement: &Stmt,
    constructor_binding_names: &HashSet<String>,
) -> bool {
    let expression_collides = |expression| {
        class_field_initializer_has_binding_collision(core, expression, constructor_binding_names)
    };
    let statement_collides =
        |statement| statement_has_binding_collision(core, statement, constructor_binding_names);
    match statement.data.as_deref() {
        Some(StmtData::Block(value)) => {
            statements_have_binding_collision(core, &value.statements, constructor_binding_names)
        }
        Some(StmtData::ExportDefault(value)) => statement_collides(&value.value),
        Some(StmtData::ExportEquals(value)) => expression_collides(&value.value),
        Some(StmtData::LazyExport(value)) => expression_collides(&value.value),
        Some(StmtData::Expr(value)) => expression_collides(&value.value),
        Some(StmtData::Enum(value)) => value
            .values
            .iter()
            .any(|item| expression_collides(&item.value_or_nil)),
        Some(StmtData::Namespace(value)) => {
            statements_have_binding_collision(core, &value.statements, constructor_binding_names)
        }
        Some(StmtData::Function(value)) => {
            function_has_binding_collision(core, &value.function, constructor_binding_names)
        }
        Some(StmtData::Class(value)) => {
            class_has_binding_collision(core, &value.class, constructor_binding_names)
        }
        Some(StmtData::Label(value)) => statement_collides(&value.statement),
        Some(StmtData::If(value)) => {
            expression_collides(&value.test)
                || statement_collides(&value.yes)
                || statement_collides(&value.no_or_nil)
        }
        Some(StmtData::For(value)) => {
            statement_collides(&value.init_or_nil)
                || expression_collides(&value.test_or_nil)
                || expression_collides(&value.update_or_nil)
                || statement_collides(&value.body)
        }
        Some(StmtData::ForIn(value)) => {
            statement_collides(&value.init)
                || expression_collides(&value.value)
                || statement_collides(&value.body)
        }
        Some(StmtData::ForOf(value)) => {
            statement_collides(&value.init)
                || expression_collides(&value.value)
                || statement_collides(&value.body)
        }
        Some(StmtData::DoWhile(value)) => {
            statement_collides(&value.body) || expression_collides(&value.test)
        }
        Some(StmtData::While(value)) => {
            expression_collides(&value.test) || statement_collides(&value.body)
        }
        Some(StmtData::With(value)) => {
            expression_collides(&value.value) || statement_collides(&value.body)
        }
        Some(StmtData::Try(value)) => {
            statements_have_binding_collision(
                core,
                &value.block.statements,
                constructor_binding_names,
            ) || value.catch.as_ref().is_some_and(|catch| {
                binding_has_binding_collision(
                    core,
                    &catch.binding_or_nil,
                    constructor_binding_names,
                ) || statements_have_binding_collision(
                    core,
                    &catch.block.statements,
                    constructor_binding_names,
                )
            }) || value.finally.as_ref().is_some_and(|finally| {
                statements_have_binding_collision(
                    core,
                    &finally.block.statements,
                    constructor_binding_names,
                )
            })
        }
        Some(StmtData::Switch(value)) => {
            expression_collides(&value.test)
                || value.cases.iter().any(|case| {
                    expression_collides(&case.value_or_nil)
                        || statements_have_binding_collision(
                            core,
                            &case.body,
                            constructor_binding_names,
                        )
                })
        }
        Some(StmtData::Return(value)) => expression_collides(&value.value_or_nil),
        Some(StmtData::Throw(value)) => expression_collides(&value.value),
        Some(StmtData::Local(value)) => value.declarations.iter().any(|declaration| {
            binding_has_binding_collision(core, &declaration.binding, constructor_binding_names)
                || expression_collides(&declaration.value_or_nil)
        }),
        Some(
            StmtData::Comment(_)
            | StmtData::Debugger
            | StmtData::Directive(_)
            | StmtData::Empty
            | StmtData::TypeScript(_)
            | StmtData::ExportClause(_)
            | StmtData::ExportFrom(_)
            | StmtData::ExportStar(_)
            | StmtData::Import(_)
            | StmtData::Break(_)
            | StmtData::Continue(_),
        )
        | None => false,
    }
}

#[allow(clippy::too_many_lines)]
fn lower_type_script_constructor_parameter_fields(core: &ParserCore, class: &mut Class) {
    if !core.options.ts.parse {
        return;
    }
    let Some(constructor_index) = class.properties.iter().position(|property| {
        property.kind == PropertyKind::Method
            && matches!(
                property.key.data.as_deref(),
                Some(ExprData::String(name))
                    if utf16_to_string(&name.value) == b"constructor"
            )
    }) else {
        return;
    };
    let is_derived = class.extends_or_nil.data.is_some();
    let use_define = class.use_define_for_class_fields;
    let mut assignments = Vec::new();
    let mut field_properties = Vec::new();
    {
        let constructor = &mut class.properties[constructor_index];
        let Some(ExprData::Function(function)) = constructor.value_or_nil.data.as_deref_mut()
        else {
            return;
        };
        for argument in &mut function.function.args {
            if !argument.is_typescript_ctor_field {
                continue;
            }
            let Some(BindingData::Identifier(identifier)) = argument.binding.data.as_deref() else {
                continue;
            };
            let loc = argument.binding.loc;
            let name = symbol_name(core, identifier.reference);
            assignments.push(Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value: Expr::new(
                        loc,
                        ExprData::Binary(BinaryExpr {
                            left: Expr::new(
                                loc,
                                ExprData::Dot(DotExpr {
                                    target: Expr::new(loc, ExprData::This),
                                    name: name.clone(),
                                    name_loc: loc,
                                    ..DotExpr::default()
                                }),
                            ),
                            right: Expr::new(
                                loc,
                                ExprData::Identifier(IdentifierExpr {
                                    reference: identifier.reference,
                                    ..IdentifierExpr::default()
                                }),
                            ),
                            op: OpCode::BinaryAssign,
                        }),
                    ),
                    ..ExprStmt::default()
                }),
            ));
            if use_define {
                field_properties.push(Property {
                    kind: PropertyKind::Field,
                    key: Expr::new(
                        loc,
                        ExprData::String(StringExpr {
                            value: string_to_utf16(name.as_bytes()),
                            ..StringExpr::default()
                        }),
                    ),
                    ..Property::default()
                });
            }
        }
        if assignments.is_empty() {
            return;
        }
        let statements = &mut function.function.body.block.statements;
        if is_derived {
            if !insert_parameter_fields_after_super(statements, &assignments) {
                return;
            }
        } else {
            statements.splice(0..0, assignments);
        }
        for argument in &mut function.function.args {
            argument.is_typescript_ctor_field = false;
        }
    }
    class.properties.extend(field_properties);
}

fn insert_parameter_fields_after_super(statements: &mut Vec<Stmt>, assignments: &[Stmt]) -> bool {
    let mut inserted = false;
    let mut index = 0;
    while index < statements.len() {
        if is_direct_super_call_statement(&statements[index]) {
            let insertion_index = index + 1;
            statements.splice(
                insertion_index..insertion_index,
                assignments.iter().cloned(),
            );
            inserted = true;
            index = insertion_index + assignments.len();
        } else {
            inserted |= insert_parameter_fields_in_statement(&mut statements[index], assignments);
            index += 1;
        }
    }
    inserted
}

#[allow(clippy::too_many_lines)]
fn insert_parameter_fields_in_statement(statement: &mut Stmt, assignments: &[Stmt]) -> bool {
    if is_direct_super_call_statement(statement) {
        let Some(StmtData::Expr(expression)) = statement.data.as_deref_mut() else {
            return false;
        };
        let mut value = std::mem::take(&mut expression.value);
        for assignment in assignments {
            let Some(StmtData::Expr(assignment)) = assignment.data.as_deref() else {
                continue;
            };
            let loc = value.loc;
            value = Expr::new(
                loc,
                ExprData::Binary(BinaryExpr {
                    left: value,
                    right: assignment.value.clone(),
                    op: OpCode::BinaryComma,
                }),
            );
        }
        expression.value = value;
        return true;
    }
    match statement.data.as_deref_mut() {
        Some(StmtData::Block(block)) => {
            insert_parameter_fields_after_super(&mut block.statements, assignments)
        }
        Some(StmtData::If(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.test, assignments)
                | insert_parameter_fields_in_statement(&mut value.yes, assignments)
                | insert_parameter_fields_in_statement(&mut value.no_or_nil, assignments)
        }
        Some(StmtData::For(value)) => {
            insert_parameter_fields_in_statement(&mut value.init_or_nil, assignments)
                | insert_parameter_fields_after_super_expression(
                    &mut value.test_or_nil,
                    assignments,
                )
                | insert_parameter_fields_after_super_expression(
                    &mut value.update_or_nil,
                    assignments,
                )
                | insert_parameter_fields_in_statement(&mut value.body, assignments)
        }
        Some(StmtData::ForIn(value)) => {
            insert_parameter_fields_in_statement(&mut value.init, assignments)
                | insert_parameter_fields_after_super_expression(&mut value.value, assignments)
                | insert_parameter_fields_in_statement(&mut value.body, assignments)
        }
        Some(StmtData::ForOf(value)) => {
            insert_parameter_fields_in_statement(&mut value.init, assignments)
                | insert_parameter_fields_after_super_expression(&mut value.value, assignments)
                | insert_parameter_fields_in_statement(&mut value.body, assignments)
        }
        Some(StmtData::DoWhile(value)) => {
            insert_parameter_fields_in_statement(&mut value.body, assignments)
                | insert_parameter_fields_after_super_expression(&mut value.test, assignments)
        }
        Some(StmtData::While(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.test, assignments)
                | insert_parameter_fields_in_statement(&mut value.body, assignments)
        }
        Some(StmtData::With(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
                | insert_parameter_fields_in_statement(&mut value.body, assignments)
        }
        Some(StmtData::Label(value)) => {
            insert_parameter_fields_in_statement(&mut value.statement, assignments)
        }
        Some(StmtData::Try(value)) => {
            let mut inserted =
                insert_parameter_fields_after_super(&mut value.block.statements, assignments);
            if let Some(catch) = &mut value.catch {
                inserted |=
                    insert_parameter_fields_after_super(&mut catch.block.statements, assignments);
            }
            if let Some(finally) = &mut value.finally {
                inserted |=
                    insert_parameter_fields_after_super(&mut finally.block.statements, assignments);
            }
            inserted
        }
        Some(StmtData::Switch(value)) => {
            let mut inserted =
                insert_parameter_fields_after_super_expression(&mut value.test, assignments);
            for case in &mut value.cases {
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut case.value_or_nil,
                    assignments,
                );
                inserted |= insert_parameter_fields_after_super(&mut case.body, assignments);
            }
            inserted
        }
        Some(StmtData::Expr(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
        }
        Some(StmtData::Return(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value_or_nil, assignments)
        }
        Some(StmtData::Throw(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
        }
        Some(StmtData::Local(value)) => {
            let mut inserted = false;
            for declaration in &mut value.declarations {
                inserted |= insert_parameter_fields_after_super_binding(
                    &mut declaration.binding,
                    assignments,
                );
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut declaration.value_or_nil,
                    assignments,
                );
            }
            inserted
        }
        Some(
            StmtData::Comment(_)
            | StmtData::Debugger
            | StmtData::Directive(_)
            | StmtData::Empty
            | StmtData::TypeScript(_)
            | StmtData::ExportClause(_)
            | StmtData::ExportFrom(_)
            | StmtData::ExportDefault(_)
            | StmtData::ExportStar(_)
            | StmtData::ExportEquals(_)
            | StmtData::LazyExport(_)
            | StmtData::Enum(_)
            | StmtData::Namespace(_)
            | StmtData::Function(_)
            | StmtData::Class(_)
            | StmtData::Import(_)
            | StmtData::Break(_)
            | StmtData::Continue(_),
        )
        | None => false,
    }
}

fn is_direct_super_call_statement(statement: &Stmt) -> bool {
    matches!(
        statement.data.as_deref(),
        Some(StmtData::Expr(expression))
            if matches!(
                expression.value.data.as_deref(),
                Some(ExprData::Call(call))
                    if matches!(call.target.data.as_deref(), Some(ExprData::Super))
            )
    )
}

#[allow(clippy::too_many_lines)]
fn insert_parameter_fields_after_super_expression(
    expression: &mut Expr,
    assignments: &[Stmt],
) -> bool {
    let is_super_call = matches!(
        expression.data.as_deref(),
        Some(ExprData::Call(call))
            if matches!(call.target.data.as_deref(), Some(ExprData::Super))
    );
    if is_super_call {
        let loc = expression.loc;
        let mut value = std::mem::take(expression);
        for assignment in assignments {
            let Some(StmtData::Expr(assignment)) = assignment.data.as_deref() else {
                continue;
            };
            value = Expr::new(
                loc,
                ExprData::Binary(BinaryExpr {
                    left: value,
                    right: assignment.value.clone(),
                    op: OpCode::BinaryComma,
                }),
            );
        }
        *expression = Expr::new(
            loc,
            ExprData::Binary(BinaryExpr {
                left: value,
                right: Expr::new(loc, ExprData::This),
                op: OpCode::BinaryComma,
            }),
        );
        return true;
    }
    match expression.data.as_deref_mut() {
        Some(ExprData::Array(value)) => {
            insert_parameter_fields_after_super_expressions(&mut value.items, assignments)
        }
        Some(ExprData::Binary(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.left, assignments)
                | insert_parameter_fields_after_super_expression(&mut value.right, assignments)
        }
        Some(ExprData::New(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.target, assignments)
                | insert_parameter_fields_after_super_expressions(&mut value.args, assignments)
        }
        Some(ExprData::Call(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.target, assignments)
                | insert_parameter_fields_after_super_expressions(&mut value.args, assignments)
        }
        Some(ExprData::Dot(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.target, assignments)
        }
        Some(ExprData::Index(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.target, assignments)
                | insert_parameter_fields_after_super_expression(&mut value.index, assignments)
        }
        Some(ExprData::Arrow(value)) => {
            let mut inserted = false;
            for argument in &mut value.args {
                inserted |=
                    insert_parameter_fields_after_super_binding(&mut argument.binding, assignments);
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut argument.default_or_nil,
                    assignments,
                );
                for decorator in &mut argument.decorators {
                    inserted |= insert_parameter_fields_after_super_expression(
                        &mut decorator.value,
                        assignments,
                    );
                }
            }
            inserted
                | insert_parameter_fields_after_super(&mut value.body.block.statements, assignments)
        }
        Some(ExprData::JsxElement(value)) => {
            let mut inserted =
                insert_parameter_fields_after_super_expression(&mut value.tag_or_nil, assignments);
            for property in &mut value.properties {
                inserted |=
                    insert_parameter_fields_after_super_expression(&mut property.key, assignments);
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut property.value_or_nil,
                    assignments,
                );
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut property.initializer_or_nil,
                    assignments,
                );
                for decorator in &mut property.decorators {
                    inserted |= insert_parameter_fields_after_super_expression(
                        &mut decorator.value,
                        assignments,
                    );
                }
            }
            inserted
                | insert_parameter_fields_after_super_expressions(
                    &mut value.nullable_children,
                    assignments,
                )
        }
        Some(ExprData::Object(value)) => {
            let mut inserted = false;
            for property in &mut value.properties {
                inserted |=
                    insert_parameter_fields_after_super_expression(&mut property.key, assignments);
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut property.value_or_nil,
                    assignments,
                );
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut property.initializer_or_nil,
                    assignments,
                );
                for decorator in &mut property.decorators {
                    inserted |= insert_parameter_fields_after_super_expression(
                        &mut decorator.value,
                        assignments,
                    );
                }
            }
            inserted
        }
        Some(ExprData::Spread(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
        }
        Some(ExprData::Template(value)) => {
            let mut inserted =
                insert_parameter_fields_after_super_expression(&mut value.tag_or_nil, assignments);
            for part in &mut value.parts {
                inserted |=
                    insert_parameter_fields_after_super_expression(&mut part.value, assignments);
            }
            inserted
        }
        Some(ExprData::InlinedEnum(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
        }
        Some(ExprData::Annotation(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
        }
        Some(ExprData::If(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.test, assignments)
                | insert_parameter_fields_after_super_expression(&mut value.yes, assignments)
                | insert_parameter_fields_after_super_expression(&mut value.no, assignments)
        }
        Some(ExprData::Unary(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
        }
        Some(ExprData::Await(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value, assignments)
        }
        Some(ExprData::Yield(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.value_or_nil, assignments)
        }
        Some(ExprData::ImportCall(value)) => {
            insert_parameter_fields_after_super_expression(&mut value.expr, assignments)
                | insert_parameter_fields_after_super_expression(
                    &mut value.options_or_nil,
                    assignments,
                )
        }
        Some(
            ExprData::Boolean(_)
            | ExprData::Super
            | ExprData::Null
            | ExprData::Undefined
            | ExprData::This
            | ExprData::NewTarget(_)
            | ExprData::ImportMeta(_)
            | ExprData::Function(_)
            | ExprData::Class(_)
            | ExprData::Identifier(_)
            | ExprData::ImportIdentifier(_)
            | ExprData::PrivateIdentifier(_)
            | ExprData::NameOfSymbol(_)
            | ExprData::JsxText(_)
            | ExprData::Missing
            | ExprData::Number(_)
            | ExprData::BigInt(_)
            | ExprData::String(_)
            | ExprData::RegExp(_)
            | ExprData::RequireString(_)
            | ExprData::RequireResolveString(_)
            | ExprData::ImportString(_),
        )
        | None => false,
    }
}

fn insert_parameter_fields_after_super_expressions(
    expressions: &mut [Expr],
    assignments: &[Stmt],
) -> bool {
    let mut inserted = false;
    for expression in expressions {
        inserted |= insert_parameter_fields_after_super_expression(expression, assignments);
    }
    inserted
}

fn insert_parameter_fields_after_super_binding(
    binding: &mut Binding,
    assignments: &[Stmt],
) -> bool {
    match binding.data.as_deref_mut() {
        Some(BindingData::Array(array)) => {
            let mut inserted = false;
            for item in &mut array.items {
                inserted |=
                    insert_parameter_fields_after_super_binding(&mut item.binding, assignments);
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut item.default_value_or_nil,
                    assignments,
                );
            }
            inserted
        }
        Some(BindingData::Object(object)) => {
            let mut inserted = false;
            for property in &mut object.properties {
                if property.is_computed {
                    inserted |= insert_parameter_fields_after_super_expression(
                        &mut property.key,
                        assignments,
                    );
                }
                inserted |=
                    insert_parameter_fields_after_super_binding(&mut property.value, assignments);
                inserted |= insert_parameter_fields_after_super_expression(
                    &mut property.default_value_or_nil,
                    assignments,
                );
            }
            inserted
        }
        Some(BindingData::Missing | BindingData::Identifier(_)) | None => false,
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
                let name = inferred_name_from_binding(core, &item.binding);
                keep_inferred_name(core, &mut item.default_value_or_nil, name);
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
                let name = inferred_name_from_binding(core, &property.value);
                keep_inferred_name(core, &mut property.default_value_or_nil, name);
            }
        }
        Some(BindingData::Missing | BindingData::Identifier(_)) | None => {}
    }
}

fn record_binding(core: &mut ParserCore, binding: &mut Binding) {
    record_binding_with_duplicates(core, binding, None);
}

fn record_binding_with_duplicates(
    core: &mut ParserCore,
    binding: &mut Binding,
    mut duplicates: Option<&mut HashMap<String, Range>>,
) {
    for_each_identifier_binding(binding, &mut |loc, identifier| {
        if !ParserCore::is_stored_name_ref(identifier.reference) {
            core.record_declared_symbol(identifier.reference);
            let symbol_index =
                usize::try_from(identifier.reference.inner_index).expect("symbol index");
            let name = core.symbols[symbol_index].original_name.clone();
            validate_binding_name(core, loc, identifier.reference);
            if let Some(duplicates) = duplicates.as_deref_mut() {
                let range = crate::internal::js_lexer::range_of_identifier(&core.source, loc);
                if duplicates.insert(name.clone(), range).is_some() {
                    core.add_error_range(
                        range,
                        format!(
                            "{name:?} cannot be bound multiple times in the same parameter list"
                        ),
                    );
                }
            }
        }
    });
}

fn validate_binding_name(core: &mut ParserCore, loc: Loc, reference: crate::internal::ast::Ref) {
    if !core.is_strict_mode() {
        return;
    }
    let symbol_index = usize::try_from(reference.inner_index).expect("symbol index");
    let name = core.symbols[symbol_index].original_name.clone();
    let text = if crate::internal::js_lexer::is_strict_mode_reserved_word(&name) {
        format!("{name:?} is a reserved word and cannot be used in strict mode")
    } else if matches!(name.as_str(), "eval" | "arguments") {
        format!("Declarations with the name {name:?} cannot be used in strict mode")
    } else {
        return;
    };
    core.add_error_range(
        crate::internal::js_lexer::range_of_identifier(&core.source, loc),
        text,
    );
}

fn function_body_use_strict(core: &ParserCore, statements: &[Stmt]) -> Option<Loc> {
    for statement in statements {
        let Some(StmtData::Expr(expression)) = statement.data.as_deref() else {
            return None;
        };
        let Some(ExprData::String(value)) = expression.value.data.as_deref() else {
            return None;
        };
        let start = usize::try_from(statement.loc.start).ok()?;
        if !matches!(core.source.contents.get(start), Some(b'\'' | b'"')) {
            return None;
        }
        if value.value == "use strict".encode_utf16().collect::<Vec<_>>() {
            return Some(statement.loc);
        }
    }
    None
}

#[allow(clippy::too_many_lines)]
fn visit_expr(core: &mut ParserCore, expression: &mut Expr, resolve_identifiers: bool) {
    visit_expr_with_target(core, expression, resolve_identifiers, AssignTarget::None);
}

fn instantiate_define_expr(
    core: &mut ParserCore,
    loc: Loc,
    define: &crate::internal::config::DefineExpr,
) -> Option<ExprData> {
    if define.constant.data.is_some() {
        let mut value = define.constant.clone();
        value.loc = loc;
        return value.data.map(|data| *data);
    }
    if define.injected_define_index.is_valid() {
        let index = define.injected_define_index.get_index();
        let reference =
            if let Some(reference) = core.generated_injected_defines.get(&index).copied() {
                reference
            } else {
                let name = core
                    .options
                    .defines
                    .as_ref()
                    .and_then(|defines| defines.injected_defines.get(index as usize))
                    .map(|define| define.name.clone())?;
                let reference = core.new_symbol(crate::internal::ast::SymbolKind::Other, name);
                core.generated_injected_defines.insert(index, reference);
                reference
            };
        core.record_usage(reference);
        return Some(ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }));
    }
    let first = define.parts.first()?;
    let result = core.find_symbol(loc, first);
    core.record_usage(result.reference);
    let mut value = Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference: result.reference,
            must_keep_due_to_with_stmt: result.is_inside_with_scope,
            ..IdentifierExpr::default()
        }),
    );
    for part in &define.parts[1..] {
        value = Expr::new(
            loc,
            ExprData::Dot(DotExpr {
                target: value,
                name: part.clone(),
                name_loc: loc,
                ..DotExpr::default()
            }),
        );
    }
    value.data.map(|data| *data)
}

fn dot_chain_parts(core: &ParserCore, expression: &Expr, tail: &str) -> Option<Vec<String>> {
    fn append(core: &ParserCore, expression: &Expr, parts: &mut Vec<String>) -> bool {
        match expression.data.as_deref() {
            Some(ExprData::Identifier(identifier)) => {
                let Some(symbol) = core.symbols.get(identifier.reference.inner_index as usize)
                else {
                    return false;
                };
                if symbol.kind != crate::internal::ast::SymbolKind::Unbound {
                    return false;
                }
                parts.push(symbol.original_name.clone());
                true
            }
            Some(ExprData::Dot(dot)) => {
                if !append(core, &dot.target, parts) {
                    return false;
                }
                parts.push(dot.name.clone());
                true
            }
            Some(ExprData::Index(index)) => {
                let Some(ExprData::String(string)) = index.index.data.as_deref() else {
                    return false;
                };
                if !append(core, &index.target, parts) {
                    return false;
                }
                parts.push(
                    String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                        &string.value,
                    ))
                    .into_owned(),
                );
                true
            }
            _ => false,
        }
    }

    let mut parts = Vec::new();
    if append(core, expression, &mut parts) {
        parts.push(tail.to_string());
        Some(parts)
    } else {
        None
    }
}

#[allow(clippy::too_many_lines)]
fn visit_expr_with_target(
    core: &mut ParserCore,
    expression: &mut Expr,
    resolve_identifiers: bool,
    assign_target: AssignTarget,
) {
    if assign_target != AssignTarget::None {
        let is_pattern = match expression.data.as_deref() {
            Some(
                ExprData::Array(_) | ExprData::Object(_) | ExprData::Spread(_) | ExprData::Missing,
            ) => true,
            Some(ExprData::Binary(binary)) => binary.op == OpCode::BinaryAssign,
            _ => false,
        };
        if !is_pattern && !core.is_valid_assignment_target(expression, core.is_strict_mode()) {
            core.add_error_range(
                Range {
                    loc: expression.loc,
                    len: 0,
                },
                "Invalid assignment target",
            );
        }
    }
    let Some(data) = expression.data.as_deref_mut() else {
        return;
    };
    let mut keep_name = None;
    match data {
        ExprData::Identifier(identifier) => {
            if !resolve_identifiers {
                return;
            }
            if ParserCore::is_stored_name_ref(identifier.reference) {
                let name = String::from_utf8_lossy(core.load_name_from_ref(identifier.reference))
                    .into_owned();
                if core.is_strict_mode()
                    && crate::internal::js_lexer::is_strict_mode_reserved_word(&name)
                {
                    core.add_error_range(
                        crate::internal::js_lexer::range_of_identifier(
                            &core.source,
                            expression.loc,
                        ),
                        format!("{name:?} is a reserved word and cannot be used in strict mode"),
                    );
                }
                let result = core.find_symbol(expression.loc, &name);
                identifier.reference = result.reference;
                identifier.must_keep_due_to_with_stmt = result.is_inside_with_scope;
            } else {
                core.record_usage(identifier.reference);
            }
            if assign_target == AssignTarget::None
                && core.options.minify_syntax
                && !identifier.must_keep_due_to_with_stmt
                && let Some(value) = core.const_values.get(&identifier.reference)
                && let Some(replacement) =
                    crate::internal::js_ast::const_value_to_expr(expression.loc, value).data
            {
                *data = *replacement;
                return;
            }
            if assign_target == AssignTarget::None {
                let symbol = &core.symbols[identifier.reference.inner_index as usize];
                if symbol.kind == crate::internal::ast::SymbolKind::Unbound
                    && let Some(define) = core
                        .options
                        .defines
                        .as_ref()
                        .and_then(|defines| defines.identifier_defines.get(&symbol.original_name))
                        .cloned()
                {
                    identifier.can_be_removed_if_unused = define
                        .flags
                        .contains(crate::internal::config::DefineFlags::CAN_BE_REMOVED_IF_UNUSED);
                    identifier.call_can_be_unwrapped_if_unused = !core
                        .options
                        .ignore_dce_annotations
                        && define.flags.contains(
                            crate::internal::config::DefineFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED,
                        );
                    if let Some(define_expr) = define.define_expr
                        && let Some(replacement) =
                            instantiate_define_expr(core, expression.loc, &define_expr)
                    {
                        *data = replacement;
                        return;
                    }
                }
            }
            if assign_target != AssignTarget::None {
                let symbol_index =
                    usize::try_from(identifier.reference.inner_index).expect("symbol index");
                let symbol_kind = core.symbols[symbol_index].kind;
                let symbol_name = core.symbols[symbol_index].original_name.clone();
                let range =
                    crate::internal::js_lexer::range_of_identifier(&core.source, expression.loc);
                match symbol_kind {
                    crate::internal::ast::SymbolKind::Const => {
                        let text =
                            format!("Cannot assign to {symbol_name:?} because it is a constant");
                        if core.options.mode == crate::internal::config::Mode::Bundle {
                            core.add_error_range(range, text);
                        } else {
                            core.add_warning_range(range, text);
                        }
                    }
                    crate::internal::ast::SymbolKind::Import => {
                        core.add_error_range(
                            range,
                            format!("Cannot assign to {symbol_name:?} because it is an import"),
                        );
                    }
                    _ => {}
                }
                core.const_values.remove(&identifier.reference);
                core.symbols[symbol_index].flags |=
                    crate::internal::ast::SymbolFlags::COULD_POTENTIALLY_BE_MUTATED;
            } else if let Some(value) = core
                .ts_enum_values_by_ref
                .get(&identifier.reference)
                .cloned()
            {
                let comment = core.symbols
                    [usize::try_from(identifier.reference.inner_index).expect("symbol index")]
                .original_name
                .clone();
                let value = if value.is_string {
                    Expr::new(
                        expression.loc,
                        ExprData::String(crate::internal::js_ast::StringExpr {
                            value: value.string,
                            ..crate::internal::js_ast::StringExpr::default()
                        }),
                    )
                } else {
                    Expr::new(expression.loc, ExprData::Number(value.number))
                };
                *data = ExprData::InlinedEnum(crate::internal::js_ast::InlinedEnumExpr {
                    value,
                    comment,
                });
            }
        }
        ExprData::ImportIdentifier(identifier) => core.record_usage(identifier.reference),
        ExprData::PrivateIdentifier(private) => {
            if ParserCore::is_stored_name_ref(private.reference) {
                let name = String::from_utf8_lossy(core.load_name_from_ref(private.reference))
                    .into_owned();
                let result = core.find_symbol(expression.loc, &name);
                private.reference = result.reference;
                let symbol_index =
                    usize::try_from(result.reference.inner_index).expect("symbol index");
                if core.symbols[symbol_index].kind == crate::internal::ast::SymbolKind::Unbound {
                    core.add_error_range(
                        crate::internal::js_lexer::range_of_identifier(
                            &core.source,
                            expression.loc,
                        ),
                        format!("Private name {name:?} must be declared in an enclosing class"),
                    );
                }
            } else {
                core.record_usage(private.reference);
            }
        }
        ExprData::Array(array) => {
            for item in &mut array.items {
                visit_expr_with_target(
                    core,
                    item,
                    resolve_identifiers,
                    if assign_target == AssignTarget::None {
                        AssignTarget::None
                    } else {
                        AssignTarget::Replace
                    },
                );
            }
        }
        ExprData::Unary(unary) => {
            if unary.op == OpCode::UnaryDelete
                && core.is_strict_mode()
                && matches!(unary.value.data.as_deref(), Some(ExprData::Identifier(_)))
            {
                core.add_error_range(
                    Range {
                        loc: expression.loc,
                        len: 6,
                    },
                    "Delete of a bare identifier cannot be used in strict mode",
                );
            }
            visit_expr_with_target(
                core,
                &mut unary.value,
                resolve_identifiers,
                unary.op.unary_assign_target(),
            );
            if core.should_fold_type_script_constant_expressions {
                let number = crate::internal::js_ast::to_number_without_side_effects(
                    unary.value.data.as_deref(),
                );
                let replacement = match unary.op {
                    OpCode::UnaryPositive => number,
                    OpCode::UnaryNegative => number.map(|value| -value),
                    OpCode::UnaryComplement => {
                        number.map(|value| f64::from(!crate::internal::js_ast::to_int32(value)))
                    }
                    _ => None,
                };
                if let Some(number) = replacement {
                    *data = ExprData::Number(number);
                }
            }
        }
        ExprData::Binary(binary) => {
            let left_target =
                if assign_target != AssignTarget::None && binary.op == OpCode::BinaryAssign {
                    AssignTarget::Replace
                } else {
                    binary.op.binary_assign_target()
                };
            visit_expr_with_target(core, &mut binary.left, resolve_identifiers, left_target);
            let inferred_name = if matches!(
                binary.op,
                OpCode::BinaryAssign
                    | OpCode::BinaryNullishCoalescingAssign
                    | OpCode::BinaryLogicalOrAssign
                    | OpCode::BinaryLogicalAndAssign
            ) {
                inferred_name_from_expression(core, &binary.left)
            } else {
                None
            };
            visit_expr(core, &mut binary.right, resolve_identifiers);
            keep_inferred_name(core, &mut binary.right, inferred_name);
            if (core.should_fold_type_script_constant_expressions
                || (core.options.minify_syntax
                    && crate::internal::js_ast::should_fold_binary_operator_when_minifying(binary)))
                && let Some(folded) =
                    crate::internal::js_ast::fold_binary_operator(expression.loc, binary)
                && let Some(folded) = folded.data
            {
                *data = *folded;
            }
        }
        ExprData::New(new) => {
            visit_expr(core, &mut new.target, resolve_identifiers);
            for argument in &mut new.args {
                visit_expr(core, argument, resolve_identifiers);
            }
        }
        ExprData::Call(call) => {
            let target_was_identifier =
                matches!(call.target.data.as_deref(), Some(ExprData::Identifier(_)));
            visit_expr(core, &mut call.target, resolve_identifiers);
            call.can_be_unwrapped_if_unused |= match call.target.data.as_deref() {
                Some(ExprData::Identifier(identifier)) => {
                    identifier.call_can_be_unwrapped_if_unused
                }
                Some(ExprData::Dot(dot)) => dot.call_can_be_unwrapped_if_unused,
                Some(ExprData::Index(index)) => index.call_can_be_unwrapped_if_unused,
                _ => false,
            };
            for argument in &mut call.args {
                visit_expr(core, argument, resolve_identifiers);
            }
            if core.options.drop_console {
                let mut parts = Vec::new();
                if append_console_method_chain(core, &call.target, &mut parts) {
                    if parts.len() == 1
                        || (parts.len() == 2
                            && matches!(parts.last().map(String::as_str), Some("call" | "apply")))
                    {
                        *data = ExprData::Undefined;
                        return;
                    }
                    replace_console_method_with_noop(core, &mut call.target);
                }
            }
            if target_was_identifier
                && call.optional_chain == crate::internal::js_ast::OptionalChain::None
                && is_identifier_named(core, &call.target, "eval")
            {
                call.kind = crate::internal::js_ast::CallKind::DirectEval;
                core.mark_current_scope_as_containing_direct_eval();
                if core.options.mode == crate::internal::config::Mode::Bundle
                    && !core.is_file_considered_esm
                {
                    core.record_usage(core.module_ref);
                    core.record_usage(core.exports_ref);
                }
            }
            let kind = if is_unbound_identifier_named(core, &call.target, "require") {
                Some(crate::internal::ast::ImportKind::Require)
            } else if let Some(ExprData::Dot(dot)) = call.target.data.as_deref()
                && dot.name == "resolve"
                && is_unbound_identifier_named(core, &dot.target, "require")
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
        ExprData::Dot(dot) => {
            visit_expr(core, &mut dot.target, resolve_identifiers);
            if assign_target == AssignTarget::None
                && let Some(parts) = dot_chain_parts(core, &dot.target, &dot.name)
                && let Some(define) = core
                    .options
                    .defines
                    .as_ref()
                    .and_then(|defines| defines.dot_defines.get(&dot.name))
                    .and_then(|defines| defines.iter().find(|define| define.key_parts == parts))
                    .cloned()
            {
                dot.can_be_removed_if_unused = define
                    .flags
                    .contains(crate::internal::config::DefineFlags::CAN_BE_REMOVED_IF_UNUSED);
                dot.call_can_be_unwrapped_if_unused = !core.options.ignore_dce_annotations
                    && define.flags.contains(
                        crate::internal::config::DefineFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED,
                    );
                dot.is_symbol_instance = define
                    .flags
                    .contains(crate::internal::config::DefineFlags::IS_SYMBOL_INSTANCE);
                if let Some(define_expr) = define.define_expr
                    && let Some(replacement) =
                        instantiate_define_expr(core, expression.loc, &define_expr)
                {
                    *data = replacement;
                    return;
                }
            }
            let replacement = if assign_target == AssignTarget::None {
                let reference = match dot.target.data.as_deref() {
                    Some(ExprData::Identifier(identifier)) => Some(identifier.reference),
                    _ => None,
                };
                reference
                    .and_then(|reference| core.ts_enums.get(&reference))
                    .and_then(|values| values.get(&dot.name))
                    .cloned()
                    .map(|value| {
                        let value = if value.is_string {
                            Expr::new(
                                expression.loc,
                                ExprData::String(crate::internal::js_ast::StringExpr {
                                    value: value.string,
                                    ..crate::internal::js_ast::StringExpr::default()
                                }),
                            )
                        } else {
                            Expr::new(expression.loc, ExprData::Number(value.number))
                        };
                        ExprData::InlinedEnum(crate::internal::js_ast::InlinedEnumExpr {
                            value,
                            comment: dot.name.clone(),
                        })
                    })
            } else {
                None
            };
            if let Some(replacement) = replacement {
                *data = replacement;
            }
        }
        ExprData::Index(index) => {
            visit_expr(core, &mut index.target, resolve_identifiers);
            visit_expr(core, &mut index.index, resolve_identifiers);
            if assign_target == AssignTarget::None
                && let Some(ExprData::String(string)) = index.index.data.as_deref()
            {
                let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                    &string.value,
                ))
                .into_owned();
                if let Some(parts) = dot_chain_parts(core, &index.target, &name)
                    && let Some(define) = core
                        .options
                        .defines
                        .as_ref()
                        .and_then(|defines| defines.dot_defines.get(&name))
                        .and_then(|defines| defines.iter().find(|define| define.key_parts == parts))
                        .cloned()
                {
                    index.can_be_removed_if_unused = define
                        .flags
                        .contains(crate::internal::config::DefineFlags::CAN_BE_REMOVED_IF_UNUSED);
                    index.call_can_be_unwrapped_if_unused = !core.options.ignore_dce_annotations
                        && define.flags.contains(
                            crate::internal::config::DefineFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED,
                        );
                    index.is_symbol_instance = define
                        .flags
                        .contains(crate::internal::config::DefineFlags::IS_SYMBOL_INSTANCE);
                    if let Some(define_expr) = define.define_expr
                        && let Some(replacement) =
                            instantiate_define_expr(core, expression.loc, &define_expr)
                    {
                        *data = replacement;
                        return;
                    }
                }
            }
            let replacement = if assign_target == AssignTarget::None {
                let reference = match index.target.data.as_deref() {
                    Some(ExprData::Identifier(identifier)) => Some(identifier.reference),
                    _ => None,
                };
                let name = match index.index.data.as_deref() {
                    Some(ExprData::String(string)) => Some(
                        String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                            &string.value,
                        ))
                        .into_owned(),
                    ),
                    _ => None,
                };
                reference
                    .zip(name)
                    .and_then(|(reference, name)| {
                        core.ts_enums
                            .get(&reference)
                            .and_then(|values| values.get(&name))
                            .cloned()
                            .map(|value| (name, value))
                    })
                    .map(|(name, value)| {
                        let value = if value.is_string {
                            Expr::new(
                                expression.loc,
                                ExprData::String(crate::internal::js_ast::StringExpr {
                                    value: value.string,
                                    ..crate::internal::js_ast::StringExpr::default()
                                }),
                            )
                        } else {
                            Expr::new(expression.loc, ExprData::Number(value.number))
                        };
                        ExprData::InlinedEnum(crate::internal::js_ast::InlinedEnumExpr {
                            value,
                            comment: name,
                        })
                    })
            } else {
                None
            };
            if let Some(replacement) = replacement {
                *data = replacement;
            }
        }
        ExprData::Object(object) => {
            report_duplicate_properties(core, &object.properties, DuplicatePropertiesIn::Object);
            if assign_target == AssignTarget::None {
                report_duplicate_proto_properties(core, &object.properties);
            }
            for property in &mut object.properties {
                if assign_target == AssignTarget::None && property.initializer_or_nil.data.is_some()
                {
                    core.add_error_range(
                        Range {
                            loc: property.initializer_or_nil.loc,
                            len: 0,
                        },
                        "Unexpected \"=\"",
                    );
                }
                if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                    visit_expr(core, &mut property.key, resolve_identifiers);
                    if core.options.minify_syntax {
                        let inlined_key = if let Some(ExprData::InlinedEnum(inlined)) =
                            property.key.data.as_deref()
                            && matches!(
                                inlined.value.data.as_deref(),
                                Some(ExprData::String(_) | ExprData::Number(_))
                            ) {
                            inlined.value.data.clone()
                        } else {
                            None
                        };
                        if let Some(inlined_key) = inlined_key {
                            property.key.data = Some(inlined_key);
                        }
                        let can_remove_computed = match property.key.data.as_deref() {
                            Some(ExprData::Number(_) | ExprData::NameOfSymbol(_)) => true,
                            Some(ExprData::String(key)) => {
                                !crate::internal::helpers::utf16_equals_wtf8(
                                    &key.value,
                                    b"__proto__",
                                )
                            }
                            _ => false,
                        };
                        if can_remove_computed {
                            property.flags.remove(PropertyFlags::IS_COMPUTED);
                        }
                    }
                }
                visit_expr_with_target(
                    core,
                    &mut property.value_or_nil,
                    resolve_identifiers,
                    if assign_target == AssignTarget::None {
                        AssignTarget::None
                    } else {
                        AssignTarget::Replace
                    },
                );
                visit_expr(core, &mut property.initializer_or_nil, resolve_identifiers);
                if property.kind == PropertyKind::Field {
                    let name = inferred_name_from_expression(core, &property.key);
                    keep_inferred_name(core, &mut property.value_or_nil, name.clone());
                    keep_inferred_name(core, &mut property.initializer_or_nil, name);
                }
                for decorator in &mut property.decorators {
                    visit_expr(core, &mut decorator.value, resolve_identifiers);
                }
            }
        }
        ExprData::Spread(spread) => visit_expr_with_target(
            core,
            &mut spread.value,
            resolve_identifiers,
            if assign_target == AssignTarget::None {
                AssignTarget::None
            } else {
                AssignTarget::Replace
            },
        ),
        ExprData::Template(template) => {
            if template.legacy_octal_loc.start > 0 {
                core.add_error_range(
                    core.source
                        .range_of_legacy_octal_escape(template.legacy_octal_loc),
                    "Legacy octal escape sequences cannot be used in template literals",
                );
            }
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
            let options = import_options(&import.options_or_nil);
            if (import.options_or_nil.data.is_none() || options.is_some())
                && let Some(ExprData::String(path)) = import.expr.data.as_deref()
            {
                let (assert_or_with, flags) = options.unwrap_or_default();
                let import_record_index = core.add_import_record(
                    crate::internal::ast::ImportKind::Dynamic,
                    import.phase,
                    core.source.range_of_string(import.expr.loc),
                    String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                        &path.value,
                    ))
                    .into_owned(),
                    flags,
                );
                core.import_records[import_record_index as usize].assert_or_with = assert_or_with;
                *data = ExprData::ImportString(crate::internal::js_ast::ImportStringExpr {
                    import_record_index,
                    close_paren_loc: import.close_paren_loc,
                });
            }
        }
        ExprData::Function(function) => {
            visit_function(core, &mut function.function, resolve_identifiers);
            if core.options.keep_names
                && let Some(name) = function.function.name
            {
                keep_name = Some(
                    core.symbols
                        [usize::try_from(name.reference.inner_index).expect("symbol index")]
                    .original_name
                    .clone(),
                );
            }
        }
        ExprData::Class(class) => {
            visit_class(core, &mut class.class, resolve_identifiers);
            if core.options.keep_names
                && let Some(name) = class.class.name
            {
                let name = symbol_name(core, name.reference);
                insert_class_name_static_block(core, &mut class.class, &name);
            }
        }
        ExprData::Arrow(arrow) => {
            let old_loop_depth = std::mem::take(&mut core.visit_loop_depth);
            let old_switch_depth = std::mem::take(&mut core.visit_switch_depth);
            core.push_next_scope_for_visit_pass(ScopeKind::FunctionArgs);
            let use_strict_loc = function_body_use_strict(core, &arrow.body.block.statements);
            if use_strict_loc.is_some() {
                crate::internal::js_ast::Scope::recursive_set_strict_mode(
                    core.current_scope.as_ref().expect("arrow arguments scope"),
                    StrictModeKind::ExplicitStrict,
                );
            }
            if let Some(loc) = use_strict_loc
                && !is_simple_parameter_list(&arrow.args, arrow.has_rest_arg)
            {
                core.add_error_range(
                    core.source.range_of_string(loc),
                    "Cannot use a \"use strict\" directive in a function with a non-simple parameter list",
                );
            }
            let mut duplicate_args = HashMap::new();
            for argument in &mut arrow.args {
                record_binding_with_duplicates(
                    core,
                    &mut argument.binding,
                    Some(&mut duplicate_args),
                );
                visit_binding_initializers(core, &mut argument.binding, resolve_identifiers);
                visit_expr(core, &mut argument.default_or_nil, resolve_identifiers);
                let name = inferred_name_from_binding(core, &argument.binding);
                keep_inferred_name(core, &mut argument.default_or_nil, name);
            }
            core.push_scope_for_visit_pass(ScopeKind::FunctionBody, arrow.body.loc);
            visit_statements(core, &mut arrow.body.block.statements, resolve_identifiers);
            core.pop_scope();
            core.pop_scope();
            core.visit_loop_depth = old_loop_depth;
            core.visit_switch_depth = old_switch_depth;
        }
        ExprData::JsxElement(element) => {
            visit_expr(core, &mut element.tag_or_nil, resolve_identifiers);
            for property in &mut element.properties {
                if property.kind != PropertyKind::Spread
                    && property.flags.contains(PropertyFlags::IS_COMPUTED)
                {
                    visit_expr(core, &mut property.key, resolve_identifiers);
                }
                visit_expr(core, &mut property.value_or_nil, resolve_identifiers);
            }
            for child in &mut element.nullable_children {
                visit_expr(core, child, resolve_identifiers);
            }
            if core.options.jsx.preserve {
                let reference = match element.tag_or_nil.data.as_deref() {
                    Some(ExprData::Identifier(identifier)) => Some(identifier.reference),
                    Some(ExprData::ImportIdentifier(identifier)) => Some(identifier.reference),
                    _ => None,
                };
                if let Some(reference) = reference {
                    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                        .flags |=
                        crate::internal::ast::SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX;
                }
            }
            if !core.options.jsx.preserve {
                let mut children = std::mem::take(&mut element.nullable_children);
                children.retain(|child| child.data.is_some());
                if element.tag_or_nil.data.is_none() {
                    element.tag_or_nil = if core.options.jsx.automatic_runtime {
                        import_jsx_symbol(
                            core,
                            expression.loc,
                            super::parser_types::JsxImport::Fragment,
                        )
                    } else {
                        instantiate_jsx_define(core, expression.loc, true, resolve_identifiers)
                    };
                }

                let mut should_use_create_element = !core.options.jsx.automatic_runtime;
                if !should_use_create_element {
                    let mut saw_spread = false;
                    for property in &element.properties {
                        if property.kind == PropertyKind::Spread {
                            saw_spread = true;
                        } else if saw_spread && property_name(property).as_deref() == Some("key") {
                            should_use_create_element = true;
                            break;
                        }
                    }
                }

                let (target, args, kind) = if should_use_create_element {
                    let mut args = vec![element.tag_or_nil.clone()];
                    if element.properties.is_empty() {
                        args.push(Expr::new(element.tag_or_nil.loc, ExprData::Null));
                    } else {
                        args.push(Expr::new(
                            element.tag_or_nil.loc,
                            ExprData::Object(ObjectExpr {
                                properties: std::mem::take(&mut element.properties),
                                is_single_line: element.is_tag_single_line,
                                ..ObjectExpr::default()
                            }),
                        ));
                    }
                    args.extend(children);
                    let target = if core.options.jsx.automatic_runtime {
                        import_jsx_symbol(
                            core,
                            expression.loc,
                            super::parser_types::JsxImport::CreateElement,
                        )
                    } else {
                        instantiate_jsx_define(core, expression.loc, false, resolve_identifiers)
                    };
                    let kind = if matches!(target.data.as_deref(), Some(ExprData::Dot(_))) {
                        CallKind::TargetWasOriginallyPropertyAccess
                    } else {
                        CallKind::Normal
                    };
                    (target, args, kind)
                } else {
                    let mut key_or_nil = None;
                    let mut properties = Vec::with_capacity(element.properties.len() + 1);
                    for property in std::mem::take(&mut element.properties) {
                        match property_name(&property).as_deref() {
                            Some("key") => {
                                if property.flags.contains(PropertyFlags::WAS_SHORTHAND) {
                                    core.add_error_range(
                                        crate::internal::logger::Range {
                                            loc: property.loc,
                                            len: 3,
                                        },
                                        "Please provide an explicit value for \"key\":",
                                    );
                                }
                                key_or_nil = Some(property.value_or_nil);
                            }
                            Some("__source" | "__self") => {
                                core.add_error_range(
                                    crate::internal::logger::Range {
                                        loc: property.loc,
                                        len: 0,
                                    },
                                    format!(
                                        "Duplicate {:?} prop found:",
                                        property_name(&property).unwrap_or_default()
                                    ),
                                );
                            }
                            _ => properties.push(property),
                        }
                    }
                    let mut is_static_children = children.len() > 1;
                    if !children.is_empty() {
                        let child_loc = children[0].loc;
                        let child = if children.len() == 1
                            && !matches!(children[0].data.as_deref(), Some(ExprData::Spread(_)))
                        {
                            children.pop().expect("one child")
                        } else {
                            if children.len() == 1 {
                                is_static_children = true;
                            }
                            Expr::new(
                                child_loc,
                                ExprData::Array(crate::internal::js_ast::ArrayExpr {
                                    items: children,
                                    ..crate::internal::js_ast::ArrayExpr::default()
                                }),
                            )
                        };
                        properties.push(crate::internal::js_ast::Property {
                            key: Expr::new(
                                child_loc,
                                ExprData::String(crate::internal::js_ast::StringExpr {
                                    value: crate::internal::helpers::string_to_utf16(b"children"),
                                    ..crate::internal::js_ast::StringExpr::default()
                                }),
                            ),
                            value_or_nil: child,
                            loc: child_loc,
                            ..crate::internal::js_ast::Property::default()
                        });
                    }
                    let mut args = vec![
                        element.tag_or_nil.clone(),
                        Expr::new(
                            element.tag_or_nil.loc,
                            ExprData::Object(ObjectExpr {
                                properties,
                                is_single_line: element.is_tag_single_line,
                                ..ObjectExpr::default()
                            }),
                        ),
                    ];
                    if core.options.jsx.development {
                        args.push(
                            key_or_nil
                                .unwrap_or_else(|| Expr::new(expression.loc, ExprData::Undefined)),
                        );
                        args.push(Expr::new(
                            expression.loc,
                            ExprData::Boolean(is_static_children),
                        ));
                        let source_location =
                            core.tracker
                                .msg_location_or_none(crate::internal::logger::Range {
                                    loc: expression.loc,
                                    len: 0,
                                });
                        let (line, column) = source_location.map_or((1.0, 1.0), |location| {
                            (
                                f64::from(u32::try_from(location.line).unwrap_or(u32::MAX)),
                                f64::from(
                                    u32::try_from(location.column.saturating_add(1))
                                        .unwrap_or(u32::MAX),
                                ),
                            )
                        });
                        let file_name = core
                            .source
                            .pretty_paths
                            .select(core.options.code_path_style)
                            .as_bytes()
                            .to_vec();
                        args.push(Expr::new(
                            expression.loc,
                            ExprData::Object(ObjectExpr {
                                properties: vec![
                                    jsx_metadata_property(
                                        expression.loc,
                                        b"fileName",
                                        ExprData::String(crate::internal::js_ast::StringExpr {
                                            value: crate::internal::helpers::string_to_utf16(
                                                &file_name,
                                            ),
                                            ..crate::internal::js_ast::StringExpr::default()
                                        }),
                                    ),
                                    jsx_metadata_property(
                                        expression.loc,
                                        b"lineNumber",
                                        ExprData::Number(line),
                                    ),
                                    jsx_metadata_property(
                                        expression.loc,
                                        b"columnNumber",
                                        ExprData::Number(column),
                                    ),
                                ],
                                ..ObjectExpr::default()
                            }),
                        ));
                        args.push(Expr::new(expression.loc, ExprData::This));
                    } else if let Some(key) = key_or_nil {
                        args.push(key);
                    }
                    let import = if is_static_children {
                        super::parser_types::JsxImport::Jsxs
                    } else {
                        super::parser_types::JsxImport::Jsx
                    };
                    (
                        import_jsx_symbol(core, expression.loc, import),
                        args,
                        CallKind::Normal,
                    )
                };
                *data = ExprData::Call(CallExpr {
                    target,
                    args,
                    close_paren_loc: element.close_loc,
                    kind,
                    is_multi_line: !element.is_tag_single_line,
                    can_be_unwrapped_if_unused: !core.options.ignore_dce_annotations
                        && !core.options.jsx.side_effects,
                    ..CallExpr::default()
                });
            }
        }
        ExprData::Boolean(_)
        | ExprData::Super
        | ExprData::Null
        | ExprData::Undefined
        | ExprData::This
        | ExprData::ImportMeta(_)
        | ExprData::NameOfSymbol(_)
        | ExprData::JsxText(_)
        | ExprData::Missing
        | ExprData::BigInt(_)
        | ExprData::RegExp(_)
        | ExprData::RequireString(_)
        | ExprData::RequireResolveString(_)
        | ExprData::ImportString(_) => {}
        ExprData::String(string) => {
            if string.legacy_octal_loc.start > 0 {
                let range = core
                    .source
                    .range_of_legacy_octal_escape(string.legacy_octal_loc);
                if string.prefer_template {
                    core.add_error_range(
                        range,
                        "Legacy octal escape sequences cannot be used in template literals",
                    );
                } else if core.is_strict_mode() {
                    core.add_error_range(
                        range,
                        "Legacy octal escape sequences cannot be used in strict mode",
                    );
                }
            }
        }
        ExprData::Number(_) => {
            if core.is_strict_mode()
                && let Some(range) = core.legacy_octal_literals.get(&expression.loc).copied()
            {
                core.add_error_range(range, "Legacy octal literals cannot be used in strict mode");
            }
        }
        ExprData::NewTarget(new_target) => {
            if !core.visit_new_target_allowed {
                core.add_error_range(new_target.range, "Cannot use \"new.target\" here:");
            }
        }
    }
    if let Some(name) = keep_name {
        let loc = expression.loc;
        let original = std::mem::take(expression);
        let mut call = core.call_runtime(
            loc,
            "__name",
            vec![
                original,
                Expr::new(
                    loc,
                    ExprData::String(StringExpr {
                        value: crate::internal::helpers::string_to_utf16(name.as_bytes()),
                        ..StringExpr::default()
                    }),
                ),
            ],
        );
        if let Some(ExprData::Call(call)) = call.data.as_deref_mut() {
            call.can_be_unwrapped_if_unused = true;
        }
        *expression = call;
    }
}

fn import_options(expression: &Expr) -> Option<(Option<ImportAssertOrWith>, ImportRecordFlags)> {
    let ExprData::Object(outer) = expression.data.as_deref()? else {
        return None;
    };
    let [property] = outer.properties.as_slice() else {
        return None;
    };
    if property.kind != PropertyKind::Field || property.flags.contains(PropertyFlags::IS_COMPUTED) {
        return None;
    }
    let ExprData::String(keyword_string) = property.key.data.as_deref()? else {
        return None;
    };
    let keyword_text = utf16_to_string(&keyword_string.value);
    let keyword = match keyword_text.as_slice() {
        b"assert" => AssertOrWithKeyword::Assert,
        b"with" => AssertOrWithKeyword::With,
        _ => return None,
    };
    let ExprData::Object(inner) = property.value_or_nil.data.as_deref()? else {
        return None;
    };
    let mut entries = Vec::with_capacity(inner.properties.len());
    let mut flags = ImportRecordFlags::default();
    for property in &inner.properties {
        if property.kind != PropertyKind::Field
            || property.flags.contains(PropertyFlags::IS_COMPUTED)
        {
            return None;
        }
        let ExprData::String(key) = property.key.data.as_deref()? else {
            return None;
        };
        let ExprData::String(value) = property.value_or_nil.data.as_deref()? else {
            return None;
        };
        if keyword == AssertOrWithKeyword::Assert
            && utf16_to_string(&key.value) == b"type"
            && utf16_to_string(&value.value) == b"json"
        {
            flags |= ImportRecordFlags::ASSERT_TYPE_JSON;
        }
        entries.push(AssertOrWithEntry {
            key: key.value.clone(),
            value: value.value.clone(),
            key_loc: property.key.loc,
            value_loc: property.value_or_nil.loc,
            prefer_quoted_key: property.flags.contains(PropertyFlags::PREFER_QUOTED_KEY),
        });
    }
    Some((
        Some(ImportAssertOrWith {
            entries,
            keyword,
            keyword_loc: property.key.loc,
            inner_open_brace_loc: property.value_or_nil.loc,
            inner_close_brace_loc: inner.close_brace_loc,
            outer_open_brace_loc: expression.loc,
            outer_close_brace_loc: outer.close_brace_loc,
        }),
        flags,
    ))
}

fn property_name(property: &crate::internal::js_ast::Property) -> Option<String> {
    let ExprData::String(string) = property.key.data.as_deref()? else {
        return None;
    };
    Some(String::from_utf16_lossy(&string.value))
}

fn jsx_metadata_property(
    loc: Loc,
    name: &[u8],
    value: ExprData,
) -> crate::internal::js_ast::Property {
    crate::internal::js_ast::Property {
        key: Expr::new(
            loc,
            ExprData::String(crate::internal::js_ast::StringExpr {
                value: crate::internal::helpers::string_to_utf16(name),
                ..crate::internal::js_ast::StringExpr::default()
            }),
        ),
        value_or_nil: Expr::new(loc, value),
        loc,
        ..crate::internal::js_ast::Property::default()
    }
}

fn import_jsx_symbol(
    core: &mut ParserCore,
    loc: Loc,
    mut import: super::parser_types::JsxImport,
) -> Expr {
    if core.options.jsx.development
        && matches!(
            import,
            super::parser_types::JsxImport::Jsx | super::parser_types::JsxImport::Jsxs
        )
    {
        import = super::parser_types::JsxImport::Jsx;
    }
    if let Some(reference) = core.jsx_imports.get(&import).copied() {
        core.record_usage(reference);
        return Expr::new(
            loc,
            ExprData::ImportIdentifier(crate::internal::js_ast::ImportIdentifierExpr {
                reference,
                was_originally_identifier: true,
                ..crate::internal::js_ast::ImportIdentifierExpr::default()
            }),
        );
    }

    let (alias, suffix) = match import {
        super::parser_types::JsxImport::Jsx if core.options.jsx.development => {
            ("jsxDEV", "/jsx-dev-runtime")
        }
        super::parser_types::JsxImport::Jsx => ("jsx", "/jsx-runtime"),
        super::parser_types::JsxImport::Jsxs => ("jsxs", "/jsx-runtime"),
        super::parser_types::JsxImport::Fragment if core.options.jsx.development => {
            ("Fragment", "/jsx-dev-runtime")
        }
        super::parser_types::JsxImport::Fragment => ("Fragment", "/jsx-runtime"),
        super::parser_types::JsxImport::CreateElement => ("createElement", ""),
    };
    let path = format!(
        "{}{suffix}",
        core.options.jsx.import_source.trim_end_matches('/')
    );
    let (import_record_index, namespace_ref) =
        if let Some(pair) = core.jsx_import_records.get(&path).copied() {
            pair
        } else {
            let import_record_index = core.add_import_record(
                crate::internal::ast::ImportKind::Stmt,
                crate::internal::ast::ImportPhase::Evaluation,
                crate::internal::logger::Range { loc, len: 0 },
                path.clone(),
                crate::internal::ast::ImportRecordFlags::default(),
            );
            let namespace_ref = core.new_symbol(
                crate::internal::ast::SymbolKind::Other,
                format!("import_{alias}"),
            );
            core.jsx_import_records
                .insert(path, (import_record_index, namespace_ref));
            (import_record_index, namespace_ref)
        };
    let reference = core.new_symbol(crate::internal::ast::SymbolKind::Import, alias);
    core.generated_named_imports.insert(
        reference,
        crate::internal::js_ast::NamedImport {
            alias: alias.into(),
            alias_loc: loc,
            namespace_ref,
            import_record_index,
            ..crate::internal::js_ast::NamedImport::default()
        },
    );
    core.jsx_imports.insert(import, reference);
    core.record_usage(reference);
    Expr::new(
        loc,
        ExprData::ImportIdentifier(crate::internal::js_ast::ImportIdentifierExpr {
            reference,
            was_originally_identifier: true,
            ..crate::internal::js_ast::ImportIdentifierExpr::default()
        }),
    )
}

fn instantiate_jsx_define(
    core: &mut ParserCore,
    loc: Loc,
    is_fragment: bool,
    resolve_identifiers: bool,
) -> Expr {
    let define = if is_fragment {
        core.options.jsx.fragment.clone()
    } else {
        core.options.jsx.factory.clone()
    };
    if define.constant.data.is_some() {
        let mut value = define.constant;
        value.loc = loc;
        visit_expr(core, &mut value, resolve_identifiers);
        return value;
    }
    let Some(first) = define.parts.first() else {
        return Expr::new(loc, ExprData::Undefined);
    };
    let reference = core.store_name_in_ref(
        crate::internal::js_lexer::MaybeSubstring::from_allocated(first.as_bytes().to_vec()),
    );
    let mut value = Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }),
    );
    for part in &define.parts[1..] {
        value = Expr::new(
            loc,
            ExprData::Dot(DotExpr {
                target: value,
                name: part.clone(),
                name_loc: loc,
                ..DotExpr::default()
            }),
        );
    }
    visit_expr(core, &mut value, resolve_identifiers);
    value
}

fn is_identifier_named(core: &ParserCore, expression: &Expr, expected: &str) -> bool {
    let Some(ExprData::Identifier(identifier)) = expression.data.as_deref() else {
        return false;
    };
    core.symbols
        .get(usize::try_from(identifier.reference.inner_index).unwrap_or(usize::MAX))
        .is_some_and(|symbol| symbol.original_name == expected)
}

fn report_duplicate_properties(
    core: &mut ParserCore,
    properties: &[crate::internal::js_ast::Property],
    context: DuplicatePropertiesIn,
) {
    for duplicate in find_duplicate_properties(properties, context) {
        let key = String::from_utf16_lossy(&duplicate.key);
        let context = match context {
            DuplicatePropertiesIn::Object => "object literal",
            DuplicatePropertiesIn::Class => "class body",
        };
        core.add_warning_range(
            Range {
                loc: duplicate.duplicate_loc,
                len: 0,
            },
            format!("Duplicate key {key:?} in {context}"),
        );
    }
}

fn report_duplicate_proto_properties(
    core: &mut ParserCore,
    properties: &[crate::internal::js_ast::Property],
) {
    let mut found = false;
    for property in properties {
        let is_proto = property.kind == crate::internal::js_ast::PropertyKind::Field
            && !property.flags.contains(PropertyFlags::IS_COMPUTED)
            && !property.flags.contains(PropertyFlags::WAS_SHORTHAND)
            && matches!(
                property.key.data.as_deref(),
                Some(ExprData::String(string))
                    if string.value == "__proto__".encode_utf16().collect::<Vec<_>>()
            );
        if is_proto {
            if found {
                core.add_error_range(
                    Range {
                        loc: property.key.loc,
                        len: 0,
                    },
                    "Cannot specify the \"__proto__\" property more than once per object",
                );
            }
            found = true;
        }
    }
}

fn is_unbound_identifier_named(core: &ParserCore, expression: &Expr, expected: &str) -> bool {
    let Some(ExprData::Identifier(identifier)) = expression.data.as_deref() else {
        return false;
    };
    core.symbols
        .get(usize::try_from(identifier.reference.inner_index).unwrap_or(usize::MAX))
        .is_some_and(|symbol| {
            symbol.original_name == expected
                && symbol.kind == crate::internal::ast::SymbolKind::Unbound
        })
}

fn append_console_method_chain(
    core: &ParserCore,
    expression: &Expr,
    parts: &mut Vec<String>,
) -> bool {
    match expression.data.as_deref() {
        Some(ExprData::Dot(dot)) => {
            if !append_console_method_chain(core, &dot.target, parts) {
                return false;
            }
            parts.push(dot.name.clone());
            true
        }
        Some(ExprData::Index(index)) => {
            let Some(ExprData::String(name)) = index.index.data.as_deref() else {
                return false;
            };
            if !append_console_method_chain(core, &index.target, parts) {
                return false;
            }
            parts.push(String::from_utf8_lossy(&utf16_to_string(&name.value)).into_owned());
            true
        }
        _ => is_unbound_identifier_named(core, expression, "console"),
    }
}

fn replace_console_method_with_noop(core: &ParserCore, expression: &mut Expr) -> bool {
    let target = match expression.data.as_deref_mut() {
        Some(ExprData::Dot(dot)) => &mut dot.target,
        Some(ExprData::Index(index)) => &mut index.target,
        _ => return false,
    };
    if is_unbound_identifier_named(core, target, "console") {
        let loc = expression.loc;
        *expression = Expr::new(
            loc,
            ExprData::Arrow(crate::internal::js_ast::ArrowExpr {
                body: crate::internal::js_ast::FunctionBody {
                    loc,
                    ..crate::internal::js_ast::FunctionBody::default()
                },
                ..crate::internal::js_ast::ArrowExpr::default()
            }),
        );
        true
    } else {
        replace_console_method_with_noop(core, target)
    }
}
