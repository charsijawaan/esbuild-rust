#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::internal::logger::{Loc, Range};
use crate::internal::{
    ast::{
        AssertOrWithEntry, AssertOrWithKeyword, GlobPattern, ImportAssertOrWith, ImportKind,
        ImportRecordFlags, Ref, SymbolFlags, SymbolKind,
    },
    compat::JsFeature,
    config::pretty_print_target_environment,
    helpers::{GlobPart, GlobWildcard, is_inside_node_modules, string_to_utf16, utf16_to_string},
    js_ast::{
        AnnotationExpr, AnnotationFlags, Arg, AssignTarget, BinaryExpr, Binding, BindingData,
        BlockStmt, CallExpr, CallKind, Class, Decl, DotExpr, Expr, ExprData, ExprStmt, ForStmt,
        Function, FunctionBody, FunctionExpr, IdentifierBinding, IdentifierExpr, IfExpr, IfStmt,
        LabelStmt, LocalKind, LocalStmt, NewExpr, ObjectExpr, OpCode, OptionalChain, PrimitiveType,
        Property, PropertyFlags, PropertyKind, ReturnStmt, ScopeKind, Stmt, StmtData,
        StmtsCanBeRemovedIfUnusedFlags, StrictModeKind, StringExpr, ThrowStmt, UnaryExpr, assign,
        convert_binding_to_expr, for_each_identifier_binding, inline_primitives_into_template,
        inline_spreads_of_array_literals, is_identifier, is_identifier_es5_and_es_next,
        is_primitive_literal, join_with_comma, known_primitive_type, make_helper_context,
        mangle_object_spread,
    },
    logger::{MsgId, MsgKind},
};

use super::duplicate_properties::{DuplicatePropertiesIn, find_duplicate_properties};
use super::{
    lower_typescript::lower_nested_type_script_statements,
    parser::{apply_keep_names_to_statements, apply_keep_names_to_type_script_namespaces},
    parser_core::ParserCore,
    standalone_helpers::{is_simple_parameter_list, is_unsightly_primitive},
    symbols::select_local_kind,
};

fn symbol_name(core: &ParserCore, reference: crate::internal::ast::Ref) -> String {
    if ParserCore::is_stored_name_ref(reference) {
        return String::from_utf8_lossy(core.load_name_from_ref(reference)).into_owned();
    }
    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
        .original_name
        .clone()
}

fn contains_closing_script_tag(text: &str) -> bool {
    text.as_bytes()
        .windows(8)
        .any(|window| window.eq_ignore_ascii_case(b"</script"))
}

fn lower_tagged_template(
    core: &mut ParserCore,
    loc: Loc,
    template: &mut crate::internal::js_ast::TemplateExpr,
) -> Expr {
    let mut cooked = vec![Expr::new(
        template.head_loc,
        ExprData::String(StringExpr {
            value: template.head_cooked.clone(),
            ..StringExpr::default()
        }),
    )];
    let mut raw = vec![Expr::new(
        template.head_loc,
        ExprData::String(StringExpr {
            value: template.head_raw.encode_utf16().collect(),
            ..StringExpr::default()
        }),
    )];
    let mut needs_raw = String::from_utf16_lossy(&template.head_cooked) != template.head_raw;
    let mut args = vec![Expr::default()];
    for part in &template.parts {
        args.push(part.value.clone());
        cooked.push(Expr::new(
            part.tail_loc,
            ExprData::String(StringExpr {
                value: part.tail_cooked.clone(),
                ..StringExpr::default()
            }),
        ));
        raw.push(Expr::new(
            part.tail_loc,
            ExprData::String(StringExpr {
                value: part.tail_raw.encode_utf16().collect(),
                ..StringExpr::default()
            }),
        ));
        needs_raw |= String::from_utf16_lossy(&part.tail_cooked) != part.tail_raw;
    }

    let cooked = Expr::new(
        template.head_loc,
        ExprData::Array(crate::internal::js_ast::ArrayExpr {
            items: cooked,
            is_single_line: true,
            ..crate::internal::js_ast::ArrayExpr::default()
        }),
    );
    let mut template_args = vec![cooked];
    if needs_raw {
        template_args.push(Expr::new(
            template.head_loc,
            ExprData::Array(crate::internal::js_ast::ArrayExpr {
                items: raw,
                is_single_line: true,
                ..crate::internal::js_ast::ArrayExpr::default()
            }),
        ));
    }
    let template_object = core.call_runtime(template.head_loc, "__template", template_args);
    let temp_ref = core.generate_top_level_temp_ref();
    core.record_usage(temp_ref);
    core.record_usage(temp_ref);
    let temp = || {
        Expr::new(
            loc,
            ExprData::Identifier(IdentifierExpr {
                reference: temp_ref,
                ..IdentifierExpr::default()
            }),
        )
    };
    args[0] = Expr::new(
        loc,
        ExprData::Binary(BinaryExpr {
            left: temp(),
            right: Expr::new(
                loc,
                ExprData::Binary(BinaryExpr {
                    left: temp(),
                    right: template_object,
                    op: OpCode::BinaryAssign,
                }),
            ),
            op: OpCode::BinaryLogicalOr,
        }),
    );
    let kind = if template.tag_was_originally_property_access {
        CallKind::TargetWasOriginallyPropertyAccess
    } else {
        CallKind::Normal
    };
    Expr::new(
        loc,
        ExprData::Call(CallExpr {
            target: std::mem::take(&mut template.tag_or_nil),
            args,
            kind,
            ..CallExpr::default()
        }),
    )
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

fn class_has_keep_name_static_block(class: &Class) -> bool {
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
        matches!(call.args.as_slice(), [first, second]
            if matches!(first.data.as_deref(), Some(ExprData::This))
                && matches!(second.data.as_deref(), Some(ExprData::String(_))))
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
                            must_not_be_merged: false,
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
        if class.class.name.is_none() && !class_has_keep_name_static_block(&class.class) {
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

pub(crate) fn visit_top_level_statements(core: &mut ParserCore, statements: &mut Vec<Stmt>) {
    visit_statements(core, statements, true);
    if core.options.minify_syntax {
        merge_adjacent_returns(core, statements);
    }
}

pub(crate) fn precompute_type_script_enum_constants(core: &mut ParserCore, statements: &[Stmt]) {
    for statement in statements {
        match statement.data.as_deref() {
            Some(StmtData::Enum(enumeration)) => {
                let enum_ref = core.follow_symbol_link(
                    core.ts_namespace_owner
                        .get(&enumeration.argument)
                        .copied()
                        .unwrap_or(enumeration.name.reference),
                );
                let mut constants = core.ts_enums.get(&enum_ref).cloned().unwrap_or_default();
                let mut next_numeric_value = 0.0;
                let mut has_numeric_value = true;
                for value in &enumeration.values {
                    let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                        &value.name,
                    ))
                    .into_owned();
                    let constant = if value.value_or_nil.data.is_none() && has_numeric_value {
                        Some(crate::internal::js_ast::TsEnumValue {
                            number: next_numeric_value,
                            ..crate::internal::js_ast::TsEnumValue::default()
                        })
                    } else {
                        type_script_enum_constant_from_expr(&value.value_or_nil)
                    };
                    if let Some(constant) = constant {
                        if constant.is_string {
                            has_numeric_value = false;
                        } else {
                            next_numeric_value = constant.number + 1.0;
                            has_numeric_value = true;
                        }
                        constants.insert(name, constant);
                    } else {
                        has_numeric_value = false;
                    }
                }
                core.ts_enums.insert(enum_ref, constants);
            }
            Some(StmtData::Namespace(namespace)) => {
                precompute_type_script_enum_constants(core, &namespace.statements);
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_lines)]
fn visit_statements(core: &mut ParserCore, statements: &mut Vec<Stmt>, resolve_identifiers: bool) {
    let old_control_flow_dead = core.is_control_flow_dead;
    for statement in statements.iter_mut() {
        let was_control_flow_dead = core.is_control_flow_dead;
        let is_top_level_scope = core.is_current_scope_module_scope();
        let preserves_const_local_prefix = matches!(
            statement.data.as_deref(),
            Some(
                StmtData::Empty
                    | StmtData::Comment(_)
                    | StmtData::Debugger
                    | StmtData::Directive(_)
                    | StmtData::TypeScript(_)
                    | StmtData::Local(_)
            )
        );
        if let Some(scope) = &core.current_scope {
            let mut scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !preserves_const_local_prefix {
                scope.is_after_const_local_prefix = true;
            }
        }
        let mut has_if_scope = false;
        let mut remove_overwritten_function = false;
        let mut prepend_to_statements = Vec::new();
        let mut append_to_statement = Vec::new();
        match statement.data.as_deref_mut() {
            Some(StmtData::Block(block)) => {
                visit_block(core, statement.loc, block, resolve_identifiers);
                if core.options.minify_syntax {
                    let close_brace_loc = block.close_brace_loc;
                    let statements = std::mem::take(&mut block.statements)
                        .into_iter()
                        .filter(|statement| {
                            !matches!(statement.data.as_deref(), None | Some(StmtData::Empty))
                        })
                        .collect();
                    *statement = super::standalone_helpers::stmts_to_single_stmt(
                        statement.loc,
                        statements,
                        close_brace_loc,
                    );
                }
            }
            Some(StmtData::Expr(expression)) => {
                let should_trim_unsightly_primitive = !core.options.minify_syntax
                    && !is_unsightly_primitive(expression.value.data.as_deref());
                visit_expr(core, &mut expression.value, resolve_identifiers);
                if should_trim_unsightly_primitive
                    && is_unsightly_primitive(expression.value.data.as_deref())
                {
                    statement.data = None;
                }
            }
            Some(StmtData::Import(import)) => {
                core.record_declared_symbol(import.namespace_ref);
                if let Some(default_name) = import.default_name {
                    core.record_declared_symbol(default_name.reference);
                }
                if let Some(items) = &import.items {
                    for item in items {
                        core.record_declared_symbol(item.name.reference);
                    }
                }
            }
            Some(StmtData::ExportEquals(export)) => {
                core.record_usage(core.module_ref);
                visit_expr(core, &mut export.value, resolve_identifiers);
            }
            Some(StmtData::LazyExport(export)) => {
                visit_expr(core, &mut export.value, resolve_identifiers);
            }
            Some(StmtData::TypeScript(_)) => {
                statement.data = None;
            }
            Some(StmtData::Local(local)) => {
                let is_const = local.kind == LocalKind::Const;
                if local.kind == LocalKind::AwaitUsing && core.visit_is_outside_fn_or_arrow {
                    let range = Range {
                        loc: statement.loc,
                        len: 5,
                    };
                    if core.is_control_flow_dead
                        && (core
                            .options
                            .unsupported_js_features
                            .contains(JsFeature::TOP_LEVEL_AWAIT)
                            || !core.options.output_format.keep_esm_import_export_syntax())
                    {
                        local.kind = LocalKind::Using;
                    } else {
                        core.live_top_level_await_keyword = range;
                        core.mark_syntax_feature(JsFeature::TOP_LEVEL_AWAIT, range);
                    }
                }
                for declaration in &mut local.declarations {
                    record_binding(core, &mut declaration.binding);
                    if is_const
                        && !core.options.ignore_dce_annotations
                        && matches!(
                            declaration.value_or_nil.data.as_deref(),
                            Some(ExprData::Arrow(arrow)) if arrow.has_no_side_effects_comment
                        )
                        || is_const
                            && !core.options.ignore_dce_annotations
                            && matches!(
                                declaration.value_or_nil.data.as_deref(),
                                Some(ExprData::Function(function))
                                    if function.function.has_no_side_effects_comment
                            )
                    {
                        if let Some(BindingData::Identifier(binding)) =
                            declaration.binding.data.as_deref()
                        {
                            core.symbols[binding.reference.inner_index as usize].flags |=
                                SymbolFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED;
                        }
                    }
                    visit_binding_initializers(core, &mut declaration.binding, resolve_identifiers);
                    visit_expr(core, &mut declaration.value_or_nil, resolve_identifiers);
                    if core.options.minify_syntax
                        && local.kind == LocalKind::Let
                        && matches!(
                            declaration.binding.data.as_deref(),
                            Some(BindingData::Identifier(_))
                        )
                        && matches!(
                            declaration.value_or_nil.data.as_deref(),
                            Some(ExprData::Undefined)
                        )
                    {
                        declaration.value_or_nil.data = None;
                    }
                    let is_in_const_local_prefix =
                        core.current_scope.as_ref().is_some_and(|scope| {
                            !scope
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .is_after_const_local_prefix
                        });
                    let mut recorded_const = false;
                    if core.options.minify_syntax
                        && is_in_const_local_prefix
                        && local.kind == crate::internal::js_ast::LocalKind::Const
                        && let Some(BindingData::Identifier(identifier)) =
                            declaration.binding.data.as_deref()
                        && core.symbols[usize::try_from(identifier.reference.inner_index)
                            .expect("symbol index")]
                        .use_count_estimate
                            == 0
                    {
                        let value =
                            crate::internal::js_ast::expr_to_const_value(&declaration.value_or_nil);
                        if value.kind != crate::internal::js_ast::ConstValueKind::None {
                            core.const_values.insert(identifier.reference, value);
                            recorded_const = true;
                        }
                    }
                    if !recorded_const
                        && declaration.value_or_nil.data.is_some()
                        && !is_safe_for_const_local_prefix(&declaration.value_or_nil)
                    {
                        if let Some(scope) = &core.current_scope {
                            scope
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .is_after_const_local_prefix = true;
                        }
                    }
                }
                local.kind =
                    select_local_kind(local.kind, &core.options, is_top_level_scope, false);
                if local.kind == LocalKind::Var && should_relocate_vars_to_top_level(core) {
                    let mut value = Expr::default();
                    for mut declaration in std::mem::take(&mut local.declarations) {
                        for_each_identifier_binding(
                            &mut declaration.binding,
                            &mut |loc, identifier| {
                                core.relocated_top_level_vars
                                    .push(crate::internal::ast::LocRef {
                                        loc,
                                        reference: identifier.reference,
                                    });
                                core.record_usage(identifier.reference);
                            },
                        );
                        if declaration.value_or_nil.data.is_some() {
                            value = join_with_comma(
                                value,
                                assign(
                                    convert_binding_to_expr(&declaration.binding, None),
                                    declaration.value_or_nil,
                                ),
                            );
                        }
                    }
                    statement.data = if value.data.is_some() {
                        Some(Box::new(StmtData::Expr(ExprStmt {
                            value,
                            ..ExprStmt::default()
                        })))
                    } else {
                        None
                    };
                }
            }
            Some(StmtData::Function(function)) => {
                if !core.options.ignore_dce_annotations
                    && function.function.has_no_side_effects_comment
                    && let Some(name) = function.function.name
                {
                    core.symbols[name.reference.inner_index as usize].flags |=
                        SymbolFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED;
                }
                has_if_scope = function.function.has_if_scope;
                if has_if_scope {
                    core.push_next_scope_for_visit_pass(ScopeKind::Block);
                }
                visit_function(core, &mut function.function, resolve_identifiers);
                mark_inlinable_function_declaration(core, &function.function);
                remove_overwritten_function = !function.is_export
                    && function.function.name.is_some_and(|name| {
                        core.symbols
                            [usize::try_from(name.reference.inner_index).expect("symbol index")]
                        .flags
                        .contains(SymbolFlags::REMOVE_OVERWRITTEN_FUNCTION_DECLARATION)
                    });
            }
            Some(StmtData::Class(class)) => {
                let pre_start = core.class_pre_statements.len();
                let post_start = core.class_post_statements.len();
                let has_experimental_class_decorators =
                    core.options.ts.config.experimental_decorators
                        == crate::internal::config::MaybeBool::True
                        && !class.class.decorators.is_empty();
                let convert_to_expression = (is_top_level_scope
                    && core.options.mode == crate::internal::config::Mode::Bundle)
                    || has_experimental_class_decorators
                    || class.class.should_lower_standard_decorators;
                let inner_name = visit_class(
                    core,
                    &mut class.class,
                    resolve_identifiers,
                    !convert_to_expression,
                );
                if convert_to_expression && let Some(name) = class.class.name {
                    let mut class_expression = class.class.clone();
                    if let Some(inner_name) = inner_name {
                        class_expression
                            .name
                            .as_mut()
                            .expect("named class expression")
                            .reference = inner_name;
                    } else {
                        class_expression.name = None;
                    }
                    statement.data = Some(Box::new(StmtData::Local(LocalStmt {
                        declarations: vec![Decl {
                            binding: Binding {
                                data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                                    reference: name.reference,
                                }))),
                                loc: name.loc,
                            },
                            value_or_nil: Expr::new(
                                statement.loc,
                                ExprData::Class(crate::internal::js_ast::ClassExpr {
                                    class: class_expression,
                                    ..crate::internal::js_ast::ClassExpr::default()
                                }),
                            ),
                        }],
                        kind: if is_top_level_scope
                            && core.options.mode == crate::internal::config::Mode::Bundle
                        {
                            LocalKind::Var
                        } else {
                            LocalKind::Let
                        },
                        is_export: class.is_export,
                        ..LocalStmt::default()
                    })));
                }
                prepend_to_statements.extend(core.class_pre_statements.drain(pre_start..));
                append_to_statement.extend(core.class_post_statements.drain(post_start..));
            }
            Some(StmtData::Enum(enumeration)) => {
                core.record_declared_symbol(enumeration.name.reference);
                let enum_ref = core.follow_symbol_link(
                    core.ts_namespace_owner
                        .get(&enumeration.argument)
                        .copied()
                        .unwrap_or(enumeration.name.reference),
                );
                core.push_scope_for_visit_pass(ScopeKind::Entry, statement.loc);
                core.record_declared_symbol(enumeration.argument);
                let mut next_numeric_value = 0.0;
                let mut has_numeric_value = true;
                let mut constants = core.ts_enums.get(&enum_ref).cloned().unwrap_or_default();
                let old_should_fold = core.should_fold_type_script_constant_expressions;
                core.should_fold_type_script_constant_expressions = true;
                for value in &mut enumeration.values {
                    visit_expr(core, &mut value.value_or_nil, resolve_identifiers);
                    let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                        &value.name,
                    ))
                    .into_owned();
                    match type_script_enum_constant_from_expr(&value.value_or_nil) {
                        Some(constant) if !constant.is_string => {
                            next_numeric_value = constant.number + 1.0;
                            if value.reference != crate::internal::ast::INVALID_REF {
                                core.ts_enum_values_by_ref
                                    .insert(value.reference, constant.clone());
                            }
                            constants.insert(name, constant);
                            has_numeric_value = true;
                        }
                        Some(constant) => {
                            if value.reference != crate::internal::ast::INVALID_REF {
                                core.ts_enum_values_by_ref
                                    .insert(value.reference, constant.clone());
                            }
                            constants.insert(name, constant);
                            has_numeric_value = false;
                        }
                        None if value.value_or_nil.data.is_none() && has_numeric_value => {
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
                        None if value.value_or_nil.data.is_none() => {
                            value.value_or_nil = Expr::new(value.loc, ExprData::Undefined);
                        }
                        None => has_numeric_value = false,
                    }
                    core.ts_enums.insert(enum_ref, constants.clone());
                }
                core.should_fold_type_script_constant_expressions = old_should_fold;
                core.pop_scope();
                core.ts_enums.insert(enum_ref, constants);
            }
            Some(StmtData::Namespace(namespace)) => {
                core.record_declared_symbol(namespace.name.reference);
                core.push_scope_for_visit_pass(ScopeKind::Entry, statement.loc);
                core.record_declared_symbol(namespace.argument);
                visit_statements(core, &mut namespace.statements, resolve_identifiers);
                for statement in &namespace.statements {
                    if let Some(StmtData::Local(local)) = statement.data.as_deref()
                        && local.is_export
                    {
                        for declaration in &local.declarations {
                            if let Some(BindingData::Identifier(identifier)) =
                                declaration.binding.data.as_deref()
                            {
                                core.const_values.remove(&identifier.reference);
                            }
                        }
                    }
                }
                lower_nested_type_script_statements(
                    core,
                    &mut namespace.statements,
                    Some(namespace.argument),
                );
                core.pop_scope();
            }
            Some(StmtData::ExportDefault(export)) => {
                core.record_declared_symbol(export.default_name.reference);
                let mut remove_type_only_default = false;
                match export.value.data.as_deref_mut() {
                    Some(StmtData::Expr(expression)) => {
                        visit_expr(core, &mut expression.value, resolve_identifiers);
                        keep_inferred_name(core, &mut expression.value, Some("default".into()));
                        if core.options.ts.parse
                            && let Some(ExprData::Identifier(identifier)) =
                                expression.value.data.as_deref()
                        {
                            let symbol =
                                &core.symbols[usize::try_from(identifier.reference.inner_index)
                                    .expect("symbol index")];
                            remove_type_only_default = symbol.kind == SymbolKind::Unbound
                                && core.local_type_names.contains(&symbol.original_name);
                        }
                    }
                    Some(StmtData::Function(function)) => {
                        if !core.options.ignore_dce_annotations
                            && function.function.has_no_side_effects_comment
                            && let Some(name) = function.function.name
                        {
                            core.symbols[name.reference.inner_index as usize].flags |=
                                SymbolFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED;
                        }
                        visit_function(core, &mut function.function, resolve_identifiers);
                    }
                    Some(StmtData::Class(class)) => {
                        if class.class.name.is_none()
                            && core.options.ts.config.experimental_decorators
                                == crate::internal::config::MaybeBool::True
                        {
                            class.class.name = Some(export.default_name);
                        }
                        let pre_start = core.class_pre_statements.len();
                        let post_start = core.class_post_statements.len();
                        let convert_to_expression = is_top_level_scope
                            && core.options.mode == crate::internal::config::Mode::Bundle;
                        let inner_name = visit_class(
                            core,
                            &mut class.class,
                            resolve_identifiers,
                            !convert_to_expression,
                        );
                        if convert_to_expression {
                            let mut class_expression = class.class.clone();
                            if let Some(inner_name) = inner_name {
                                if let Some(name) = &mut class_expression.name {
                                    name.reference = inner_name;
                                }
                            } else {
                                class_expression.name = None;
                            }
                            export.value = Stmt::new(
                                export.value.loc,
                                StmtData::Expr(ExprStmt {
                                    value: Expr::new(
                                        export.value.loc,
                                        ExprData::Class(crate::internal::js_ast::ClassExpr {
                                            class: class_expression,
                                            ..crate::internal::js_ast::ClassExpr::default()
                                        }),
                                    ),
                                    ..ExprStmt::default()
                                }),
                            );
                        }
                        prepend_to_statements.extend(core.class_pre_statements.drain(pre_start..));
                        append_to_statement.extend(core.class_post_statements.drain(post_start..));
                    }
                    _ => {}
                }
                if remove_type_only_default {
                    statement.data = None;
                    continue;
                }
                let symbol_index = usize::try_from(export.default_name.reference.inner_index)
                    .expect("symbol index");
                if core.symbols[symbol_index].original_name == "default" {
                    core.symbols[symbol_index].original_name =
                        format!("{}_default", core.source.identifier_name);
                }
            }
            Some(StmtData::If(if_statement)) => {
                visit_expr(core, &mut if_statement.test, resolve_identifiers);
                let old_control_flow_dead = core.is_control_flow_dead;
                let constant = match crate::internal::js_ast::to_boolean_with_side_effects(
                    if_statement.test.data.as_deref(),
                ) {
                    Some((value, crate::internal::js_ast::SideEffects::NoSideEffects)) => {
                        Some(value)
                    }
                    _ => None,
                };
                validate_single_statement(core, &if_statement.yes, SingleStatementContext::If);
                core.is_control_flow_dead = old_control_flow_dead || constant == Some(false);
                visit_statement(core, &mut if_statement.yes, resolve_identifiers);
                validate_single_statement(
                    core,
                    &if_statement.no_or_nil,
                    SingleStatementContext::If,
                );
                core.is_control_flow_dead = old_control_flow_dead || constant == Some(true);
                visit_statement(core, &mut if_statement.no_or_nil, resolve_identifiers);
                core.is_control_flow_dead = old_control_flow_dead;
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
                if core.options.minify_syntax {
                    optimize_loop_body(core, &mut loop_statement.body);
                }
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
                if core.options.minify_syntax {
                    optimize_loop_body(core, &mut loop_statement.body);
                }
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
                if core.options.minify_syntax
                    && !core.visit_is_async_generator
                    && matches!(
                        return_statement.value_or_nil.data.as_deref(),
                        Some(ExprData::Undefined)
                    )
                {
                    return_statement.value_or_nil.data = None;
                }
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
                if core.options.minify_syntax {
                    optimize_loop_body(core, &mut loop_statement.body);
                }
                core.pop_scope();
            }
            Some(StmtData::ForIn(loop_statement)) => {
                core.push_scope_for_visit_pass(ScopeKind::Block, statement.loc);
                let var_initializer = match loop_statement.init.data.as_deref() {
                    Some(StmtData::Local(local))
                        if local.kind == LocalKind::Var && local.declarations.len() == 1 =>
                    {
                        let declaration = &local.declarations[0];
                        match declaration.binding.data.as_deref() {
                            Some(BindingData::Identifier(identifier))
                                if declaration.value_or_nil.data.is_some() =>
                            {
                                Some((declaration.binding.loc, identifier.reference))
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                visit_for_loop_init(core, &mut loop_statement.init, resolve_identifiers, true);
                if let Some((binding_loc, reference)) = var_initializer {
                    let assignment = match loop_statement.init.data.as_deref_mut() {
                        Some(StmtData::Expr(expression)) => {
                            let assignment = std::mem::take(&mut expression.value);
                            loop_statement.init = Stmt::new(
                                binding_loc,
                                StmtData::Expr(ExprStmt {
                                    value: Expr::new(
                                        binding_loc,
                                        ExprData::Identifier(IdentifierExpr {
                                            reference,
                                            ..IdentifierExpr::default()
                                        }),
                                    ),
                                    ..ExprStmt::default()
                                }),
                            );
                            assignment
                        }
                        Some(StmtData::Local(local)) => {
                            let initializer =
                                std::mem::take(&mut local.declarations[0].value_or_nil);
                            assign(
                                Expr::new(
                                    binding_loc,
                                    ExprData::Identifier(IdentifierExpr {
                                        reference,
                                        ..IdentifierExpr::default()
                                    }),
                                ),
                                initializer,
                            )
                        }
                        _ => Expr::default(),
                    };
                    if assignment.data.is_some() {
                        prepend_to_statements.push(Stmt::new(
                            statement.loc,
                            StmtData::Expr(ExprStmt {
                                value: assignment,
                                ..ExprStmt::default()
                            }),
                        ));
                    }
                }
                visit_expr(core, &mut loop_statement.value, resolve_identifiers);
                validate_single_statement(
                    core,
                    &loop_statement.body,
                    SingleStatementContext::Other,
                );
                core.visit_loop_depth += 1;
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                core.visit_loop_depth -= 1;
                if core.options.minify_syntax {
                    optimize_loop_body(core, &mut loop_statement.body);
                }
                relocate_for_in_or_of_init(core, &mut loop_statement.init);
                core.pop_scope();
            }
            Some(StmtData::ForOf(loop_statement)) => {
                if loop_statement.await_range.len > 0 && core.visit_is_outside_fn_or_arrow {
                    if core.is_control_flow_dead
                        && (core
                            .options
                            .unsupported_js_features
                            .contains(JsFeature::TOP_LEVEL_AWAIT)
                            || !core.options.output_format.keep_esm_import_export_syntax())
                    {
                        loop_statement.await_range = Range::default();
                    } else {
                        core.live_top_level_await_keyword = loop_statement.await_range;
                        core.mark_syntax_feature(
                            JsFeature::TOP_LEVEL_AWAIT,
                            loop_statement.await_range,
                        );
                    }
                }
                core.push_scope_for_visit_pass(ScopeKind::Block, statement.loc);
                visit_for_loop_init(core, &mut loop_statement.init, resolve_identifiers, true);
                visit_expr(core, &mut loop_statement.value, resolve_identifiers);
                validate_single_statement(
                    core,
                    &loop_statement.body,
                    SingleStatementContext::Other,
                );
                core.visit_loop_depth += 1;
                visit_statement(core, &mut loop_statement.body, resolve_identifiers);
                core.visit_loop_depth -= 1;
                if core.options.minify_syntax {
                    optimize_loop_body(core, &mut loop_statement.body);
                }
                relocate_for_in_or_of_init(core, &mut loop_statement.init);
                core.pop_scope();
            }
            Some(StmtData::Label(_)) => {
                visit_label_statement_chain(core, statement, resolve_identifiers);
            }
            Some(StmtData::Switch(switch)) => {
                visit_expr(core, &mut switch.test, resolve_identifiers);
                core.push_scope_for_visit_pass(ScopeKind::Block, switch.body_loc);
                core.visit_switch_depth += 1;
                for case in &mut switch.cases {
                    visit_expr(core, &mut case.value_or_nil, resolve_identifiers);
                }

                let mut duplicate_cases: HashMap<u32, Vec<Expr>> = HashMap::new();
                for case in &switch.cases {
                    if let Some(hash) = super::control_flow::duplicate_case_hash(&case.value_or_nil)
                    {
                        let entries = duplicate_cases.entry(hash).or_default();
                        if let Some((earlier, could_be_incorrect)) =
                            entries.iter().find_map(|earlier| {
                                let (equal, could_be_incorrect) =
                                    super::control_flow::duplicate_case_equals(
                                        earlier,
                                        &case.value_or_nil,
                                    );
                                equal.then_some((earlier, could_be_incorrect))
                            })
                            && let Some(log) = &core.log
                        {
                            let range = |expression: &Expr| {
                                if matches!(expression.data.as_deref(), Some(ExprData::String(_))) {
                                    core.source.range_of_string(expression.loc)
                                } else {
                                    core.source
                                        .range_of_operator_before(expression.loc, b"case")
                                }
                            };
                            let later_range = range(&case.value_or_nil);
                            let earlier_range = range(earlier);
                            let text = if could_be_incorrect {
                                "This case clause may never be evaluated because it likely duplicates an earlier case clause"
                            } else {
                                "This case clause will never be evaluated because it duplicates an earlier case clause"
                            };
                            let note = core
                                .tracker
                                .msg_data(earlier_range, "The earlier case clause is here:");
                            log.add_id_with_notes(
                                MsgId::JsDuplicateCase,
                                if is_inside_node_modules(&core.source.key_path.text) {
                                    MsgKind::Debug
                                } else {
                                    MsgKind::Warning
                                },
                                Some(&mut core.tracker),
                                later_range,
                                text,
                                vec![note],
                            );
                        }
                        entries.push(case.value_or_nil.clone());
                    }
                }

                let liveness = super::control_flow::analyze_switch_cases_for_liveness(switch);
                for (case, liveness) in switch.cases.iter_mut().zip(liveness) {
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
                    let old_control_flow_dead = core.is_control_flow_dead;
                    if liveness.status == super::control_flow::LivenessStatus::AlwaysDead {
                        core.is_control_flow_dead = true;
                    }
                    visit_statements(core, &mut case.body, resolve_identifiers);
                    core.is_control_flow_dead = old_control_flow_dead;
                    lower_block_level_function_declarations(core, &mut case.body);
                    lower_nested_type_script_statements(core, &mut case.body, None);
                }
                core.visit_switch_depth -= 1;
                core.pop_scope();
            }
            Some(StmtData::Try(try_statement)) => {
                if core.visit_try_body_depth == 0 {
                    core.visit_try_catch_loc = try_statement
                        .catch
                        .as_ref()
                        .map_or(statement.loc, |catch| catch.loc);
                }
                core.visit_try_body_depth += 1;
                visit_block(
                    core,
                    try_statement.block_loc,
                    &mut try_statement.block,
                    resolve_identifiers,
                );
                core.visit_try_body_depth -= 1;
                if let Some(catch) = &mut try_statement.catch {
                    let old_control_flow_dead = core.is_control_flow_dead;
                    let catch_is_dead = try_statement
                        .block
                        .statements
                        .iter()
                        .all(|statement| statement.data.is_none());
                    core.is_control_flow_dead |= catch_is_dead;
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
                    core.is_control_flow_dead = old_control_flow_dead;
                }
                if let Some(finally) = &mut try_statement.finally {
                    core.push_next_scope_for_visit_pass(ScopeKind::Block);
                    visit_statements(core, &mut finally.block.statements, resolve_identifiers);
                    lower_block_level_function_declarations(core, &mut finally.block.statements);
                    lower_nested_type_script_statements(core, &mut finally.block.statements, None);
                    core.pop_scope();
                }
                if core.options.minify_syntax {
                    let try_is_empty = try_statement
                        .block
                        .statements
                        .iter()
                        .all(|statement| statement.data.is_none());
                    if try_is_empty {
                        let keep_catch = try_statement.catch.as_mut().is_some_and(|catch| {
                            catch.block.statements.iter_mut().any(
                                super::dead_control_flow::should_keep_stmt_in_dead_control_flow,
                            )
                        });
                        if !keep_catch {
                            statement.data = try_statement.finally.take().map(|finally| {
                                if super::control_flow::stmts_care_about_scope(
                                    &finally.block.statements,
                                ) {
                                    Box::new(StmtData::Block(finally.block))
                                } else {
                                    super::standalone_helpers::stmts_to_single_stmt(
                                        finally.loc,
                                        finally.block.statements,
                                        finally.block.close_brace_loc,
                                    )
                                    .data
                                    .unwrap_or_else(|| Box::new(StmtData::Empty))
                                }
                            });
                        }
                    } else if try_statement.finally.as_ref().is_some_and(|finally| {
                        finally
                            .block
                            .statements
                            .iter()
                            .all(|statement| statement.data.is_none())
                    }) {
                        if try_statement.catch.is_some() {
                            try_statement.finally = None;
                        } else if let Some(finally) = try_statement.finally.take() {
                            statement.data = Some(
                                if super::control_flow::stmts_care_about_scope(
                                    &try_statement.block.statements,
                                ) {
                                    Box::new(StmtData::Block(std::mem::take(
                                        &mut try_statement.block,
                                    )))
                                } else {
                                    super::standalone_helpers::stmts_to_single_stmt(
                                        finally.loc,
                                        std::mem::take(&mut try_statement.block.statements),
                                        try_statement.block.close_brace_loc,
                                    )
                                    .data
                                    .unwrap_or_else(|| Box::new(StmtData::Empty))
                                },
                            );
                        }
                    }
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
        if !prepend_to_statements.is_empty() || !append_to_statement.is_empty() {
            let loc = statement.loc;
            let original = std::mem::take(statement);
            let mut expanded =
                Vec::with_capacity(prepend_to_statements.len() + append_to_statement.len() + 1);
            expanded.append(&mut prepend_to_statements);
            expanded.push(original);
            expanded.append(&mut append_to_statement);
            *statement = Stmt::new(
                loc,
                StmtData::Block(BlockStmt {
                    statements: expanded,
                    close_brace_loc: Loc { start: -2 },
                }),
            );
        }
        if remove_overwritten_function {
            statement.data = None;
            if has_if_scope {
                core.pop_scope();
            }
            continue;
        }
        if has_if_scope {
            let loc = statement.loc;
            let mut lowered = vec![std::mem::take(statement)];
            lower_block_level_function_declarations(core, &mut lowered);
            *statement =
                super::standalone_helpers::stmts_to_single_stmt(loc, lowered, Loc::default());
            core.pop_scope();
        }
        if core.options.tree_shaking
            && !core.options.minify_syntax
            && let Some(StmtData::Expr(expression)) = statement.data.as_deref_mut()
            && matches!(
                expression.value.data.as_deref(),
                Some(ExprData::Call(call)) if call.can_be_unwrapped_if_unused
            )
        {
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
        if core.options.minify_syntax {
            if matches!(statement.data.as_deref(), Some(StmtData::Empty)) {
                statement.data = None;
                continue;
            }
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
    }
    core.is_control_flow_dead = old_control_flow_dead;
    flatten_synthetic_statement_blocks(statements);
    if core.options.minify_syntax {
        if !old_control_flow_dead {
            inline_single_use_declarations(core, statements);
        }
        for statement in statements.iter_mut() {
            let Some(StmtData::Expr(expression)) = statement.data.as_deref_mut() else {
                continue;
            };
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
        absorb_expressions_into_for_initializers(statements);
        merge_adjacent_throws(core, statements);
        flatten_blocks_without_scope(statements);
        remove_dead_statements_after_jumps(statements);
    }
}

fn flatten_synthetic_statement_blocks(statements: &mut Vec<Stmt>) {
    let mut flattened = Vec::with_capacity(statements.len());
    for statement in std::mem::take(statements) {
        if let Some(StmtData::Block(block)) = statement.data.as_deref()
            && block.close_brace_loc.start == -2
        {
            flattened.extend(block.statements.iter().cloned());
        } else {
            flattened.push(statement);
        }
    }
    *statements = flattened;
}

fn flatten_blocks_without_scope(statements: &mut Vec<Stmt>) {
    let mut flattened = Vec::with_capacity(statements.len());
    for statement in std::mem::take(statements) {
        if let Some(StmtData::Block(block)) = statement.data.as_deref()
            && !super::control_flow::stmts_care_about_scope(&block.statements)
            && block
                .statements
                .iter()
                .rev()
                .find(|statement| statement.data.is_some())
                .is_some_and(|statement| {
                    matches!(
                        statement.data.as_deref(),
                        Some(
                            StmtData::Return(_)
                                | StmtData::Throw(_)
                                | StmtData::Break(_)
                                | StmtData::Continue(_)
                        )
                    )
                })
        {
            flattened.extend(
                block
                    .statements
                    .iter()
                    .filter(|statement| statement.data.is_some())
                    .cloned(),
            );
        } else {
            flattened.push(statement);
        }
    }
    *statements = flattened;
}

fn is_safe_for_const_local_prefix(expression: &Expr) -> bool {
    match expression.data.as_deref() {
        Some(
            ExprData::Missing
            | ExprData::String(_)
            | ExprData::RegExp(_)
            | ExprData::BigInt(_)
            | ExprData::Function(_)
            | ExprData::Arrow(_),
        ) => true,
        Some(ExprData::Array(array)) => array.items.iter().all(is_safe_for_const_local_prefix),
        Some(ExprData::Object(object)) => object.properties.is_empty(),
        _ => false,
    }
}

fn should_relocate_vars_to_top_level(core: &ParserCore) -> bool {
    if core.options.mode != crate::internal::config::Mode::Bundle {
        return false;
    }
    let (Some(mut scope), Some(module_scope)) =
        (core.current_scope.clone(), core.module_scope.as_ref())
    else {
        return false;
    };
    if std::sync::Arc::ptr_eq(&scope, module_scope) && core.single_statement_depth == 0 {
        return false;
    }
    loop {
        let (kind, parent) = {
            let scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (scope.kind, scope.parent.clone())
        };
        if kind.stops_hoisting() {
            return std::sync::Arc::ptr_eq(&scope, module_scope);
        }
        let Some(parent) = parent.and_then(|parent| parent.upgrade()) else {
            return false;
        };
        scope = parent;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubstituteStatus {
    Continue,
    Success,
    Failure,
}

fn expression_can_be_removed_if_unused(core: &ParserCore, expression: &Expr) -> bool {
    make_helper_context(|reference| {
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
            == SymbolKind::Unbound
    })
    .expr_can_be_removed_if_unused(expression)
}

fn lower_object_spread(
    core: &mut ParserCore,
    loc: Loc,
    object: &mut ObjectExpr,
) -> Option<ExprData> {
    if !core
        .options
        .unsupported_js_features
        .contains(JsFeature::OBJECT_REST_SPREAD)
        || !object
            .properties
            .iter()
            .any(|property| property.kind == PropertyKind::Spread)
    {
        return None;
    }

    let is_single_line = object.is_single_line;
    let close_brace_loc = object.close_brace_loc;
    let mut result = Expr::default();
    let mut properties = Vec::new();
    for mut property in std::mem::take(&mut object.properties) {
        if property.kind != PropertyKind::Spread {
            properties.push(property);
            continue;
        }

        if !properties.is_empty() || result.data.is_none() {
            let next = Expr::new(
                loc,
                ExprData::Object(ObjectExpr {
                    properties: std::mem::take(&mut properties),
                    is_single_line,
                    ..ObjectExpr::default()
                }),
            );
            result = if result.data.is_none() {
                next
            } else {
                core.call_runtime(loc, "__spreadProps", vec![result, next])
            };
        }
        result = core.call_runtime(
            loc,
            "__spreadValues",
            vec![result, std::mem::take(&mut property.value_or_nil)],
        );
    }

    if !properties.is_empty() {
        let trailing = Expr::new(
            loc,
            ExprData::Object(ObjectExpr {
                properties,
                close_brace_loc,
                is_single_line,
                ..ObjectExpr::default()
            }),
        );
        result = core.call_runtime(loc, "__spreadProps", vec![result, trailing]);
    }
    result.data.map(|data| *data)
}

fn lower_object_spread_expression(core: &mut ParserCore, expression: &mut Expr) {
    let replacement = match expression.data.as_deref_mut() {
        Some(ExprData::Object(object)) => lower_object_spread(core, expression.loc, object),
        _ => None,
    };
    if let Some(replacement) = replacement {
        expression.data = Some(Box::new(replacement));
    }
}

fn substitute_single_use_symbol_in_statement(
    core: &mut ParserCore,
    statement: &mut Stmt,
    reference: Ref,
    replacement: &Expr,
) -> bool {
    let expression = match statement.data.as_deref_mut() {
        Some(StmtData::Expr(statement)) => Some(&mut statement.value),
        Some(StmtData::Throw(statement)) => Some(&mut statement.value),
        Some(StmtData::Return(statement))
            if !matches!(
                statement.value_or_nil.data.as_deref(),
                Some(ExprData::Unary(unary)) if unary.op == OpCode::UnaryVoid
            ) =>
        {
            Some(&mut statement.value_or_nil)
        }
        Some(StmtData::If(statement)) => Some(&mut statement.test),
        Some(StmtData::Switch(statement)) => Some(&mut statement.test),
        Some(StmtData::Local(statement)) => statement
            .declarations
            .first_mut()
            .filter(|declaration| {
                declaration.value_or_nil.data.is_some()
                    && matches!(
                        declaration.binding.data.as_deref(),
                        Some(BindingData::Identifier(_))
                    )
            })
            .map(|declaration| &mut declaration.value_or_nil),
        _ => None,
    };
    let Some(expression) = expression else {
        return false;
    };
    let replacement_can_be_removed = expression_can_be_removed_if_unused(core, replacement);
    substitute_single_use_symbol_in_expression(
        core,
        expression,
        reference,
        replacement,
        replacement_can_be_removed,
    ) == SubstituteStatus::Success
}

#[allow(clippy::too_many_lines)]
fn substitute_single_use_symbol_in_expression(
    core: &mut ParserCore,
    expression: &mut Expr,
    reference: Ref,
    replacement: &Expr,
    replacement_can_be_removed: bool,
) -> SubstituteStatus {
    let expression_loc = expression.loc;
    match expression.data.as_deref_mut() {
        Some(ExprData::Identifier(identifier)) if identifier.reference == reference => {
            core.ignore_usage(reference);
            *expression = replacement.clone();
            return SubstituteStatus::Success;
        }
        Some(ExprData::Spread(spread)) => {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut spread.value,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
        }
        Some(ExprData::Await(await_expression)) => {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut await_expression.value,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
        }
        Some(ExprData::Yield(yield_expression)) if yield_expression.value_or_nil.data.is_some() => {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut yield_expression.value_or_nil,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
        }
        Some(ExprData::ImportCall(import_call)) => {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut import_call.expr,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
            if replacement_can_be_removed
                && expression_can_be_removed_if_unused(core, &import_call.expr)
            {
                return SubstituteStatus::Continue;
            }
        }
        Some(ExprData::Unary(unary))
            if unary.op.unary_assign_target() == AssignTarget::None
                && unary.op != OpCode::UnaryDelete =>
        {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut unary.value,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
        }
        Some(ExprData::Dot(dot)) => {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut dot.target,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
        }
        Some(ExprData::Binary(binary)) => {
            let assign_target = binary.op.binary_assign_target();
            if assign_target == AssignTarget::None {
                let status = substitute_single_use_symbol_in_expression(
                    core,
                    &mut binary.left,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if status != SubstituteStatus::Continue {
                    return status;
                }
            } else if !expression_can_be_removed_if_unused(core, &binary.left)
                || (assign_target == AssignTarget::Update && !replacement_can_be_removed)
            {
                return SubstituteStatus::Failure;
            }
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut binary.right,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
        }
        Some(ExprData::If(conditional)) => {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut conditional.test,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
            if replacement_can_be_removed {
                let yes_status = substitute_single_use_symbol_in_expression(
                    core,
                    &mut conditional.yes,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if yes_status == SubstituteStatus::Success {
                    return yes_status;
                }
                let no_status = substitute_single_use_symbol_in_expression(
                    core,
                    &mut conditional.no,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if no_status == SubstituteStatus::Success {
                    return no_status;
                }
                if yes_status != SubstituteStatus::Continue
                    || no_status != SubstituteStatus::Continue
                {
                    return SubstituteStatus::Failure;
                }
            }
        }
        Some(ExprData::Index(index)) => {
            let status = substitute_single_use_symbol_in_expression(
                core,
                &mut index.target,
                reference,
                replacement,
                replacement_can_be_removed,
            );
            if status != SubstituteStatus::Continue {
                return status;
            }
            if replacement_can_be_removed || index.optional_chain == OptionalChain::None {
                let status = substitute_single_use_symbol_in_expression(
                    core,
                    &mut index.index,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if status != SubstituteStatus::Continue {
                    return status;
                }
            }
        }
        Some(ExprData::Call(call)) => {
            let replacement_is_property_access = matches!(
                replacement.data.as_deref(),
                Some(ExprData::Dot(_) | ExprData::Index(_))
            );
            let target_is_direct_reference = matches!(
                call.target.data.as_deref(),
                Some(ExprData::Identifier(identifier)) if identifier.reference == reference
            );
            let target_is_indirect_reference = matches!(
                call.target.data.as_deref(),
                Some(ExprData::Binary(binary))
                    if binary.op == OpCode::BinaryComma
                        && matches!(
                            binary.right.data.as_deref(),
                            Some(ExprData::Identifier(identifier))
                                if identifier.reference == reference
                        )
            );
            let target_is_reference = target_is_direct_reference || target_is_indirect_reference;
            if !(replacement_is_property_access && target_is_reference) {
                let status = substitute_single_use_symbol_in_expression(
                    core,
                    &mut call.target,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if status != SubstituteStatus::Continue {
                    if status == SubstituteStatus::Success {
                        if target_is_indirect_reference
                            && let Some(ExprData::Binary(binary)) = call.target.data.as_deref_mut()
                            && binary.op == OpCode::BinaryComma
                        {
                            let right_is_unbound_eval = matches!(
                                binary.right.data.as_deref(),
                                Some(ExprData::Identifier(identifier))
                                    if {
                                        let symbol = &core.symbols[usize::try_from(
                                            identifier.reference.inner_index,
                                        )
                                        .expect("symbol index")];
                                        symbol.kind == SymbolKind::Unbound
                                            && symbol.original_name == "eval"
                                    }
                            );
                            if !right_is_unbound_eval {
                                call.target = std::mem::take(&mut binary.right);
                            }
                        }
                        if let Some(inlined) = maybe_inline_iife(expression_loc, call) {
                            expression.data = Some(Box::new(inlined));
                        } else if let Some(ExprData::Identifier(identifier)) =
                            call.target.data.as_deref()
                        {
                            let symbol =
                                &core.symbols[usize::try_from(identifier.reference.inner_index)
                                    .expect("symbol index")];
                            if symbol.kind == SymbolKind::Unbound
                                && symbol.original_name == "eval"
                                && call.kind != CallKind::DirectEval
                            {
                                let target = std::mem::take(&mut call.target);
                                call.target = Expr::new(
                                    target.loc,
                                    ExprData::Binary(BinaryExpr {
                                        left: Expr::new(target.loc, ExprData::Number(0.0)),
                                        right: target,
                                        op: OpCode::BinaryComma,
                                    }),
                                );
                            }
                        }
                    }
                    return status;
                }
                if replacement_can_be_removed || call.optional_chain == OptionalChain::None {
                    for argument in &mut call.args {
                        let status = substitute_single_use_symbol_in_expression(
                            core,
                            argument,
                            reference,
                            replacement,
                            replacement_can_be_removed,
                        );
                        if status != SubstituteStatus::Continue {
                            return status;
                        }
                    }
                }
            }
        }
        Some(ExprData::Array(array)) => {
            for item in &mut array.items {
                let status = substitute_single_use_symbol_in_expression(
                    core,
                    item,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if status != SubstituteStatus::Continue {
                    return status;
                }
            }
        }
        Some(ExprData::Object(object)) => {
            for property in &mut object.properties {
                if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                    let status = substitute_single_use_symbol_in_expression(
                        core,
                        &mut property.key,
                        reference,
                        replacement,
                        replacement_can_be_removed,
                    );
                    if status != SubstituteStatus::Continue {
                        return status;
                    }
                    return SubstituteStatus::Failure;
                }
                if property.value_or_nil.data.is_some() {
                    let status = substitute_single_use_symbol_in_expression(
                        core,
                        &mut property.value_or_nil,
                        reference,
                        replacement,
                        replacement_can_be_removed,
                    );
                    if status != SubstituteStatus::Continue {
                        return status;
                    }
                }
            }
        }
        Some(ExprData::Template(template)) => {
            if template.tag_or_nil.data.is_some() {
                let status = substitute_single_use_symbol_in_expression(
                    core,
                    &mut template.tag_or_nil,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if status != SubstituteStatus::Continue {
                    return status;
                }
            }
            for part in &mut template.parts {
                let status = substitute_single_use_symbol_in_expression(
                    core,
                    &mut part.value,
                    reference,
                    replacement,
                    replacement_can_be_removed,
                );
                if status != SubstituteStatus::Continue {
                    if status == SubstituteStatus::Success
                        && is_primitive_literal(part.value.data.as_deref())
                    {
                        *expression = inline_primitives_into_template(expression_loc, template);
                    }
                    return status;
                }
            }
        }
        Some(ExprData::InlinedEnum(inlined)) => {
            return substitute_single_use_symbol_in_expression(
                core,
                &mut inlined.value,
                reference,
                replacement,
                replacement_can_be_removed,
            );
        }
        Some(ExprData::Annotation(annotation)) => {
            return substitute_single_use_symbol_in_expression(
                core,
                &mut annotation.value,
                reference,
                replacement,
                replacement_can_be_removed,
            );
        }
        _ => {}
    }
    if replacement_can_be_removed && expression_can_be_removed_if_unused(core, expression) {
        return SubstituteStatus::Continue;
    }
    if is_primitive_literal(expression.data.as_deref())
        || is_primitive_literal(replacement.data.as_deref())
    {
        return SubstituteStatus::Continue;
    }
    SubstituteStatus::Failure
}

fn inline_single_use_declarations(core: &mut ParserCore, statements: &mut [Stmt]) {
    let is_nested_scope = !core.is_current_scope_module_scope();
    let contains_direct_eval = core.current_scope.as_ref().is_some_and(|scope| {
        scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_direct_eval
    });
    if !is_nested_scope || contains_direct_eval {
        return;
    }
    for statement in statements.iter_mut() {
        let Some(StmtData::Local(local)) = statement.data.as_deref_mut() else {
            continue;
        };
        if local.is_export {
            continue;
        }
        local.declarations.retain(|declaration| {
            let Some(BindingData::Identifier(identifier)) = declaration.binding.data.as_deref()
            else {
                return true;
            };
            !core.const_values.contains_key(&identifier.reference)
                || core.symbols
                    [usize::try_from(identifier.reference.inner_index).expect("symbol index")]
                .use_count_estimate
                    != 0
        });
        if local.declarations.is_empty() {
            statement.data = None;
        }
    }
    for statement_index in 0..statements.len() {
        while let Some(previous_index) = (0..statement_index)
            .rev()
            .find(|&index| statements[index].data.is_some())
        {
            let candidate = match statements[previous_index].data.as_deref() {
                Some(StmtData::Local(local))
                    if !local.is_export
                        && matches!(local.kind, LocalKind::Let | LocalKind::Const) =>
                {
                    local.declarations.last().and_then(|declaration| {
                        let BindingData::Identifier(identifier) =
                            declaration.binding.data.as_deref()?
                        else {
                            return None;
                        };
                        let symbol =
                            &core.symbols[usize::try_from(identifier.reference.inner_index)
                                .expect("symbol index")];
                        if symbol.use_count_estimate != 1
                            || symbol.flags.contains(SymbolFlags::DID_KEEP_NAME)
                            || symbol.flags.contains(SymbolFlags::WAS_EXPORTED)
                        {
                            return None;
                        }
                        let replacement = if declaration.value_or_nil.data.is_some() {
                            declaration.value_or_nil.clone()
                        } else {
                            Expr::new(declaration.binding.loc, ExprData::Undefined)
                        };
                        Some((identifier.reference, replacement))
                    })
                }
                _ => None,
            };
            let Some((reference, replacement)) = candidate else {
                break;
            };
            if !substitute_single_use_symbol_in_statement(
                core,
                &mut statements[statement_index],
                reference,
                &replacement,
            ) {
                break;
            }
            let Some(StmtData::Local(local)) = statements[previous_index].data.as_deref_mut()
            else {
                unreachable!("single-use candidate must still be a local declaration");
            };
            local.declarations.pop();
            if local.declarations.is_empty() {
                statements[previous_index].data = None;
            }
        }
    }
}

fn cleanup_function_body_tail(core: &ParserCore, statements: &mut [Stmt]) {
    let Some(last) = statements
        .iter_mut()
        .rev()
        .find(|statement| statement.data.is_some())
    else {
        return;
    };
    let Some(StmtData::Return(return_statement)) = last.data.as_deref() else {
        return;
    };
    if return_statement.value_or_nil.data.is_none() {
        last.data = None;
        return;
    }
    let Some(ExprData::Unary(unary)) = return_statement.value_or_nil.data.as_deref() else {
        return;
    };
    if unary.op != OpCode::UnaryVoid {
        return;
    }
    let helpers = make_helper_context(|reference| {
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
            == SymbolKind::Unbound
    });
    let value = helpers.simplify_unused_expr(&unary.value, core.options.unsupported_js_features);
    if value.data.is_some() {
        last.data = Some(Box::new(StmtData::Expr(ExprStmt {
            value,
            ..ExprStmt::default()
        })));
    } else {
        last.data = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImplicitJumpKind {
    Return,
    Continue,
}

fn is_implicit_jump(statement: &Stmt, kind: ImplicitJumpKind) -> bool {
    match (kind, statement.data.as_deref()) {
        (ImplicitJumpKind::Return, Some(StmtData::Return(statement))) => {
            statement.value_or_nil.data.is_none()
        }
        (ImplicitJumpKind::Continue, Some(StmtData::Continue(statement))) => {
            statement.label.is_none()
        }
        _ => false,
    }
}

pub(crate) fn merge_adjacent_expression_statements(statements: Vec<Stmt>) -> Vec<Stmt> {
    let mut result: Vec<Stmt> = Vec::with_capacity(statements.len());
    for statement in statements {
        let Some(data) = statement.data.as_deref() else {
            continue;
        };
        if let Some(StmtData::Expr(previous)) =
            result.last_mut().and_then(|item| item.data.as_deref_mut())
            && let StmtData::Expr(current) = data
            && !previous.must_not_be_merged
            && !current.must_not_be_merged
        {
            previous.value = join_with_comma(previous.value.clone(), current.value.clone());
            previous.is_from_class_or_fn_that_can_be_removed_if_unused &=
                current.is_from_class_or_fn_that_can_be_removed_if_unused;
            continue;
        }
        result.push(statement);
    }
    result
}

fn mangle_implicit_jump_if(core: &ParserCore, loc: Loc, test: Expr, yes: Stmt) -> Stmt {
    let helpers = make_helper_context(|reference| {
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
            == SymbolKind::Unbound
    });
    match yes.data.as_deref() {
        Some(StmtData::Expr(expression)) => {
            let value = if let Some(ExprData::Binary(comma)) = test.data.as_deref()
                && comma.op == OpCode::BinaryComma
            {
                join_with_comma(
                    comma.left.clone(),
                    Expr::new(
                        comma.right.loc,
                        ExprData::Binary(BinaryExpr {
                            left: comma.right.clone(),
                            right: expression.value.clone(),
                            op: OpCode::BinaryLogicalAnd,
                        }),
                    ),
                )
            } else if let Some(ExprData::Unary(unary)) = test.data.as_deref()
                && unary.op == OpCode::UnaryNot
            {
                Expr::new(
                    loc,
                    ExprData::Binary(BinaryExpr {
                        left: unary.value.clone(),
                        right: expression.value.clone(),
                        op: OpCode::BinaryLogicalOr,
                    }),
                )
            } else {
                Expr::new(
                    loc,
                    ExprData::Binary(BinaryExpr {
                        left: test,
                        right: expression.value.clone(),
                        op: OpCode::BinaryLogicalAnd,
                    }),
                )
            };
            let value = helpers.simplify_unused_expr(&value, core.options.unsupported_js_features);
            if value.data.is_some() {
                Stmt::new(
                    loc,
                    StmtData::Expr(ExprStmt {
                        value,
                        ..ExprStmt::default()
                    }),
                )
            } else {
                Stmt::default()
            }
        }
        Some(StmtData::Empty) | None => {
            let value = helpers.simplify_unused_expr(&test, core.options.unsupported_js_features);
            if value.data.is_some() {
                Stmt::new(
                    loc,
                    StmtData::Expr(ExprStmt {
                        value,
                        ..ExprStmt::default()
                    }),
                )
            } else {
                Stmt::default()
            }
        }
        _ => Stmt::new(
            loc,
            StmtData::If(IfStmt {
                test,
                yes,
                ..IfStmt::default()
            }),
        ),
    }
}

fn optimize_implicit_jumps(core: &ParserCore, statements: &mut [Stmt], kind: ImplicitJumpKind) {
    for statement_index in 0..statements.len() {
        let Some(StmtData::If(statement)) = statements[statement_index].data.as_deref() else {
            continue;
        };
        let statement = statement.clone();
        if !is_implicit_jump(&statement.yes, kind) {
            continue;
        }

        let mut body = Vec::new();
        if statement.no_or_nil.data.is_some() {
            body = super::control_flow::append_if_or_label_body_preserving_scope(
                body,
                statement.no_or_nil.clone(),
            );
        }
        body.extend(statements[statement_index + 1..].iter().cloned());
        if super::control_flow::stmts_care_about_scope(&body) {
            continue;
        }

        optimize_implicit_jumps(core, &mut body, kind);
        let mut body = merge_adjacent_expression_statements(body);
        merge_adjacent_returns(core, &mut body);
        body.retain(|statement| statement.data.is_some());
        let helpers = make_helper_context(|reference| {
            core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                == SymbolKind::Unbound
        });
        let mut inverted = helpers.simplify_boolean_expr(&Expr::new(
            statement.test.loc,
            ExprData::Unary(UnaryExpr {
                value: statement.test.clone(),
                op: OpCode::UnaryNot,
                ..UnaryExpr::default()
            }),
        ));
        if let Some(previous_index) = (0..statement_index)
            .rev()
            .find(|&index| statements[index].data.is_some())
            && let Some(StmtData::Expr(previous)) = statements[previous_index].data.as_deref()
        {
            inverted = join_with_comma(previous.value.clone(), inverted);
            statements[previous_index].data = None;
        }
        let yes = super::standalone_helpers::stmts_to_single_stmt(
            statement.yes.loc,
            body,
            Loc::default(),
        );
        statements[statement_index] =
            mangle_implicit_jump_if(core, statements[statement_index].loc, inverted, yes);
        for statement in &mut statements[statement_index + 1..] {
            statement.data = None;
        }
        return;
    }
}

fn trim_trailing_continue(statement: &mut Stmt) {
    match statement.data.as_deref_mut() {
        Some(StmtData::Continue(continue_statement)) if continue_statement.label.is_none() => {
            statement.data = Some(Box::new(StmtData::Empty));
        }
        Some(StmtData::Block(block)) => {
            if let Some(last) = block
                .statements
                .iter_mut()
                .rev()
                .find(|statement| statement.data.is_some())
                && matches!(
                    last.data.as_deref(),
                    Some(StmtData::Continue(statement)) if statement.label.is_none()
                )
            {
                last.data = None;
            }
        }
        _ => {}
    }
}

fn optimize_loop_body(core: &ParserCore, statement: &mut Stmt) {
    if let Some(StmtData::Block(block)) = statement.data.as_deref_mut() {
        optimize_implicit_jumps(core, &mut block.statements, ImplicitJumpKind::Continue);
    }
    trim_trailing_continue(statement);
}

fn return_value_or_undefined(statement: &Stmt) -> Option<Expr> {
    let StmtData::Return(statement) = statement.data.as_deref()? else {
        return None;
    };
    Some(if statement.value_or_nil.data.is_some() {
        statement.value_or_nil.clone()
    } else {
        Expr::new(statement.value_or_nil.loc, ExprData::Undefined)
    })
}

fn collapse_if_with_return_branches(core: &ParserCore, statement: &mut Stmt) -> bool {
    let Some(StmtData::If(if_statement)) = statement.data.as_deref() else {
        return false;
    };
    if if_statement.no_or_nil.data.is_none() {
        return false;
    }
    let Some(yes) = return_value_or_undefined(&if_statement.yes) else {
        return false;
    };
    let Some(no) = return_value_or_undefined(&if_statement.no_or_nil) else {
        return false;
    };
    let helpers = make_helper_context(|reference| {
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
            == SymbolKind::Unbound
    });
    let test = helpers.simplify_boolean_expr(&if_statement.test);
    let value = helpers.mangle_if_expr(
        test.loc,
        &IfExpr { test, yes, no },
        core.options.unsupported_js_features,
    );
    statement.data = Some(Box::new(StmtData::Return(ReturnStmt {
        value_or_nil: value,
    })));
    true
}

fn remove_dead_statements_after_jumps(statements: &mut [Stmt]) {
    let mut is_dead = false;
    for statement in statements.iter_mut() {
        if is_dead && !super::dead_control_flow::should_keep_stmt_in_dead_control_flow(statement) {
            statement.data = None;
            continue;
        }
        if matches!(
            statement.data.as_deref(),
            Some(
                StmtData::Return(_)
                    | StmtData::Throw(_)
                    | StmtData::Break(_)
                    | StmtData::Continue(_)
            )
        ) {
            is_dead = true;
        }
    }
}

fn merge_adjacent_returns(core: &ParserCore, statements: &mut [Stmt]) {
    for statement in statements.iter_mut() {
        collapse_if_with_return_branches(core, statement);
    }
    remove_dead_statements_after_jumps(statements);

    loop {
        let mut non_empty = statements
            .iter()
            .enumerate()
            .filter(|(_, statement)| statement.data.is_some())
            .map(|(index, _)| index)
            .rev();
        let Some(last_index) = non_empty.next() else {
            break;
        };
        let Some(previous_index) = non_empty.next() else {
            break;
        };
        let Some(StmtData::Return(last_return)) = statements[last_index].data.as_deref().cloned()
        else {
            break;
        };

        let replacement = match statements[previous_index].data.as_deref() {
            Some(StmtData::Expr(previous)) if last_return.value_or_nil.data.is_some() => {
                Some(ReturnStmt {
                    value_or_nil: join_with_comma(previous.value.clone(), last_return.value_or_nil),
                })
            }
            Some(StmtData::If(previous)) if previous.no_or_nil.data.is_none() => {
                let Some(yes) = return_value_or_undefined(&previous.yes) else {
                    break;
                };
                let no = if last_return.value_or_nil.data.is_some() {
                    last_return.value_or_nil
                } else {
                    Expr::new(statements[last_index].loc, ExprData::Undefined)
                };
                let helpers = make_helper_context(|reference| {
                    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                        == SymbolKind::Unbound
                });
                let test = helpers.simplify_boolean_expr(&previous.test);
                let value = helpers.mangle_if_expr(
                    test.loc,
                    &IfExpr { test, yes, no },
                    core.options.unsupported_js_features,
                );
                Some(ReturnStmt {
                    value_or_nil: value,
                })
            }
            _ => None,
        };
        let Some(replacement) = replacement else {
            break;
        };
        statements[previous_index].data = Some(Box::new(StmtData::Return(replacement)));
        statements[last_index].data = None;
    }
}

fn throw_value(statement: &Stmt) -> Option<Expr> {
    let StmtData::Throw(statement) = statement.data.as_deref()? else {
        return None;
    };
    Some(statement.value.clone())
}

fn collapse_if_with_throw_branches(core: &ParserCore, statement: &mut Stmt) -> bool {
    let Some(StmtData::If(if_statement)) = statement.data.as_deref() else {
        return false;
    };
    if if_statement.no_or_nil.data.is_none() {
        return false;
    }
    let Some(yes) = throw_value(&if_statement.yes) else {
        return false;
    };
    let Some(no) = throw_value(&if_statement.no_or_nil) else {
        return false;
    };
    let helpers = make_helper_context(|reference| {
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
            == SymbolKind::Unbound
    });
    let test = helpers.simplify_boolean_expr(&if_statement.test);
    let value = helpers.mangle_if_expr(
        test.loc,
        &IfExpr { test, yes, no },
        core.options.unsupported_js_features,
    );
    statement.data = Some(Box::new(StmtData::Throw(ThrowStmt { value })));
    true
}

fn merge_adjacent_throws(core: &ParserCore, statements: &mut [Stmt]) {
    for statement in statements.iter_mut() {
        collapse_if_with_throw_branches(core, statement);
    }
    remove_dead_statements_after_jumps(statements);

    loop {
        let mut non_empty = statements
            .iter()
            .enumerate()
            .filter(|(_, statement)| statement.data.is_some())
            .map(|(index, _)| index)
            .rev();
        let Some(last_index) = non_empty.next() else {
            break;
        };
        let Some(previous_index) = non_empty.next() else {
            break;
        };
        let Some(StmtData::Throw(last_throw)) = statements[last_index].data.as_deref().cloned()
        else {
            break;
        };

        let replacement = match statements[previous_index].data.as_deref() {
            Some(StmtData::Expr(previous)) => Some(ThrowStmt {
                value: join_with_comma(previous.value.clone(), last_throw.value),
            }),
            Some(StmtData::If(previous)) if previous.no_or_nil.data.is_none() => {
                let Some(yes) = throw_value(&previous.yes) else {
                    break;
                };
                let helpers = make_helper_context(|reference| {
                    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                        == SymbolKind::Unbound
                });
                let test = helpers.simplify_boolean_expr(&previous.test);
                let value = helpers.mangle_if_expr(
                    test.loc,
                    &IfExpr {
                        test,
                        yes,
                        no: last_throw.value,
                    },
                    core.options.unsupported_js_features,
                );
                Some(ThrowStmt { value })
            }
            _ => None,
        };
        let Some(replacement) = replacement else {
            break;
        };
        statements[previous_index].data = Some(Box::new(StmtData::Throw(replacement)));
        statements[last_index].data = None;
    }
}

fn collapse_expression_statements_into_return(statements: &mut Vec<Stmt>) -> bool {
    let Some(StmtData::Return(return_statement)) = statements
        .last()
        .and_then(|statement| statement.data.as_deref())
    else {
        return false;
    };
    if return_statement.value_or_nil.data.is_none()
        || !statements[..statements.len() - 1].iter().all(|statement| {
            matches!(
                statement.data.as_deref(),
                Some(StmtData::Expr(expression))
                    if !expression.is_from_class_or_fn_that_can_be_removed_if_unused
            )
        })
    {
        return false;
    }
    let loc = statements
        .first()
        .map_or_else(Loc::default, |statement| statement.loc);
    let mut combined = statements[..statements.len() - 1]
        .iter()
        .filter_map(|statement| match statement.data.as_deref() {
            Some(StmtData::Expr(expression)) => Some(expression.value.clone()),
            _ => None,
        })
        .fold(Expr::default(), |left, right| {
            if left.data.is_none() {
                right
            } else {
                join_with_comma(left, right)
            }
        });
    combined = if combined.data.is_none() {
        return_statement.value_or_nil.clone()
    } else {
        join_with_comma(combined, return_statement.value_or_nil.clone())
    };
    *statements = vec![Stmt::new(
        loc,
        StmtData::Return(crate::internal::js_ast::ReturnStmt {
            value_or_nil: combined,
        }),
    )];
    true
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
    let mut non_empty = block
        .statements
        .iter()
        .filter(|statement| statement.data.is_some());
    let Some(first) = non_empty.next() else {
        return Stmt::new(statement.loc, StmtData::Empty);
    };
    if non_empty.next().is_none() && !super::control_flow::stmt_cares_about_scope(first) {
        return first.clone();
    }
    statement
}

fn prepare_if_statement_for_minification(value: &mut IfStmt) {
    value.yes = unwrap_single_statement_block(std::mem::take(&mut value.yes));
    if value.no_or_nil.data.is_some() {
        value.no_or_nil = unwrap_single_statement_block(std::mem::take(&mut value.no_or_nil));
    }
    if matches!(value.yes.data.as_deref(), Some(StmtData::Empty) | None)
        && value.no_or_nil.data.is_some()
    {
        if let Some(ExprData::Unary(unary)) = value.test.data.as_deref()
            && unary.op == OpCode::UnaryNot
        {
            value.test = unary.value.clone();
        } else {
            value.test = Expr::new(
                value.test.loc,
                ExprData::Unary(UnaryExpr {
                    value: value.test.clone(),
                    op: OpCode::UnaryNot,
                    ..UnaryExpr::default()
                }),
            );
        }
        value.yes = std::mem::take(&mut value.no_or_nil);
    }
    if value.no_or_nil.data.is_none()
        && let Some(StmtData::If(inner)) = value.yes.data.as_deref()
        && inner.no_or_nil.data.is_none()
    {
        value.test = Expr::new(
            value.test.loc,
            ExprData::Binary(BinaryExpr {
                left: value.test.clone(),
                right: inner.test.clone(),
                op: OpCode::BinaryLogicalAnd,
            }),
        );
        value.yes = inner.yes.clone();
    }
}

fn minify_control_flow_statement(statement: &mut Stmt) {
    if let Some(StmtData::If(value)) = statement.data.as_deref_mut() {
        prepare_if_statement_for_minification(value);
    }
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
            let test_or_nil = if matches!(
                crate::internal::js_ast::to_boolean_with_side_effects(value.test.data.as_deref()),
                Some((true, crate::internal::js_ast::SideEffects::NoSideEffects))
            ) {
                Expr::default()
            } else {
                value.test.clone()
            };
            statement.data = Some(Box::new(StmtData::For(ForStmt {
                test_or_nil,
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
        Some(StmtData::Switch(value)) if value.cases.len() == 1 => {
            let case = &value.cases[0];
            if case.value_or_nil.data.is_none() {
                return;
            }
            let Some(body) = super::standalone_helpers::try_to_inline_case_body(
                value.body_loc,
                case.body.clone(),
                value.close_brace_loc,
            ) else {
                return;
            };
            let test = match crate::internal::js_ast::check_equality_if_no_side_effects(
                value.test.data.as_deref(),
                case.value_or_nil.data.as_deref(),
                crate::internal::js_ast::EqualityKind::Strict,
            ) {
                Some(value) => Expr::new(statement.loc, ExprData::Boolean(value)),
                None => Expr::new(
                    value.test.loc,
                    ExprData::Binary(BinaryExpr {
                        left: value.test.clone(),
                        right: case.value_or_nil.clone(),
                        op: OpCode::BinaryStrictEqual,
                    }),
                ),
            };
            statement.data = Some(Box::new(StmtData::If(IfStmt {
                test,
                yes: super::standalone_helpers::stmts_to_single_stmt(
                    case.loc,
                    body,
                    value.close_brace_loc,
                ),
                ..IfStmt::default()
            })));
            minify_control_flow_statement(statement);
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
    mut statement: &Stmt,
    mut context: SingleStatementContext,
) {
    while let Some(StmtData::Label(label)) = statement.data.as_deref() {
        statement = &label.statement;
        context = if context == SingleStatementContext::Label {
            SingleStatementContext::Label
        } else {
            SingleStatementContext::Other
        };
    }
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
        _ => {}
    }
}

struct LabelVisitFrame {
    loc: Loc,
    name: crate::internal::ast::LocRef,
    is_single_line_stmt: bool,
    reference: crate::internal::ast::Ref,
    should_drop: bool,
    old_control_flow_dead: bool,
}

fn visit_label_statement_chain(
    core: &mut ParserCore,
    statement: &mut Stmt,
    resolve_identifiers: bool,
) {
    let mut current = std::mem::take(statement);
    let mut frames = Vec::new();

    while let Some(data) = current.data.take() {
        let StmtData::Label(mut label) = *data else {
            current.data = Some(data);
            visit_statement(core, &mut current, resolve_identifiers);
            break;
        };

        validate_single_statement(core, &label.statement, SingleStatementContext::Label);
        core.push_scope_for_visit_pass(ScopeKind::Label, current.loc);
        let name =
            String::from_utf8_lossy(core.load_name_from_ref(label.name.reference)).into_owned();
        let should_drop = core.options.drop_labels.contains(&name);
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
        let old_control_flow_dead = core.is_control_flow_dead;
        if should_drop {
            core.is_control_flow_dead = true;
        }
        frames.push(LabelVisitFrame {
            loc: current.loc,
            name: label.name,
            is_single_line_stmt: label.is_single_line_stmt,
            reference,
            should_drop,
            old_control_flow_dead,
        });
        current = label.statement;
    }

    while let Some(frame) = frames.pop() {
        core.is_control_flow_dead = frame.old_control_flow_dead;
        core.pop_scope();
        if frame.should_drop {
            current = Stmt::new(frame.loc, StmtData::Empty);
        } else if core.options.minify_syntax
            && core.symbols[usize::try_from(frame.reference.inner_index).expect("symbol index")]
                .use_count_estimate
                == 0
        {
            if current.data.is_some() {
                let mut replacements =
                    super::control_flow::append_if_or_label_body_preserving_scope(
                        Vec::new(),
                        current,
                    );
                current = if replacements.is_empty() {
                    Stmt::default()
                } else if replacements.len() == 1 {
                    replacements.pop().expect("single replacement")
                } else {
                    Stmt::new(
                        frame.loc,
                        StmtData::Block(BlockStmt {
                            statements: replacements,
                            ..BlockStmt::default()
                        }),
                    )
                };
            }
        } else {
            current = Stmt::new(
                frame.loc,
                StmtData::Label(LabelStmt {
                    statement: current,
                    name: frame.name,
                    is_single_line_stmt: frame.is_single_line_stmt,
                }),
            );
        }
    }
    *statement = current;
}

fn report_forbidden_single_statement(core: &mut ParserCore, loc: Loc) {
    core.add_error_range(
        crate::internal::js_lexer::range_of_identifier(&core.source, loc),
        "Cannot use a declaration in a single-statement context",
    );
}

fn visit_statement(core: &mut ParserCore, statement: &mut Stmt, resolve_identifiers: bool) {
    if statement.data.is_none() {
        return;
    }
    let loc = statement.loc;
    let mut statements = vec![std::mem::take(statement)];
    core.single_statement_depth += 1;
    visit_statements(core, &mut statements, resolve_identifiers);
    core.single_statement_depth -= 1;
    statements.retain(|statement| statement.data.is_some());
    *statement = if statements.len() == 1 {
        statements.pop().expect("single statement")
    } else {
        super::standalone_helpers::stmts_to_single_stmt(loc, statements, Loc::default())
    };
}

fn visit_for_loop_init(
    core: &mut ParserCore,
    statement: &mut Stmt,
    resolve_identifiers: bool,
    is_in_or_of: bool,
) {
    match statement.data.as_deref_mut() {
        Some(StmtData::Expr(expression)) => visit_expr_with_target(
            core,
            &mut expression.value,
            resolve_identifiers,
            if is_in_or_of {
                AssignTarget::Replace
            } else {
                AssignTarget::None
            },
        ),
        Some(StmtData::Local(_)) => {
            let old_mode = core.options.mode;
            if is_in_or_of {
                // For-in/of declarations use a different relocation mode that
                // preserves the binding as the loop assignment target.
                core.options.mode = crate::internal::config::Mode::PassThrough;
            }
            visit_statement(core, statement, resolve_identifiers);
            core.options.mode = old_mode;
            if is_in_or_of && let Some(StmtData::Local(local)) = statement.data.as_deref_mut() {
                local.kind = select_local_kind(local.kind, &core.options, false, false);
            }
        }
        _ => panic!("Internal error: invalid for-loop initializer"),
    }
}

fn relocate_for_in_or_of_init(core: &mut ParserCore, statement: &mut Stmt) {
    if !should_relocate_vars_to_top_level(core) {
        return;
    }
    let Some(StmtData::Local(local)) = statement.data.as_deref_mut() else {
        return;
    };
    if local.kind != LocalKind::Var {
        return;
    }
    let mut value = Expr::default();
    for mut declaration in std::mem::take(&mut local.declarations) {
        for_each_identifier_binding(&mut declaration.binding, &mut |loc, identifier| {
            core.relocated_top_level_vars
                .push(crate::internal::ast::LocRef {
                    loc,
                    reference: identifier.reference,
                });
            core.record_usage(identifier.reference);
        });
        value = join_with_comma(value, convert_binding_to_expr(&declaration.binding, None));
    }
    statement.data = Some(Box::new(StmtData::Expr(ExprStmt {
        value,
        ..ExprStmt::default()
    })));
}

fn identifier_binding(loc: Loc, reference: Ref) -> Binding {
    Binding {
        data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
            reference,
        }))),
        loc,
    }
}

fn keep_block_function_name_statement(
    core: &mut ParserCore,
    loc: Loc,
    reference: Ref,
    name: &str,
) -> Stmt {
    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].flags |=
        SymbolFlags::DID_KEEP_NAME;
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
                            value: crate::internal::helpers::string_to_utf16(name.as_bytes()),
                            ..StringExpr::default()
                        }),
                    ),
                ],
            ),
            is_from_class_or_fn_that_can_be_removed_if_unused: true,
            must_not_be_merged: false,
        }),
    )
}

fn preserve_block_functions_with_direct_eval(core: &mut ParserCore, functions: &[Stmt]) {
    for statement in functions {
        let Some(StmtData::Function(function)) = statement.data.as_deref() else {
            unreachable!("block function classification changed");
        };
        let name = function
            .function
            .name
            .expect("function declarations have names");
        if let Some(&hoisted_reference) = core
            .hoisted_ref_for_sloppy_mode_block_fn
            .get(&name.reference)
        {
            core.symbols[usize::try_from(hoisted_reference.inner_index).expect("symbol index")]
                .link = name.reference;
        }
    }
}

fn lower_block_functions_to_declarations(
    core: &mut ParserCore,
    functions: Vec<Stmt>,
    visited: Vec<Stmt>,
) -> Vec<Stmt> {
    let mut declaration_index_by_reference: HashMap<Ref, usize> = HashMap::new();
    let mut lexical_declarations: Vec<Decl> = Vec::new();
    let mut keep_name_statements = Vec::new();
    let mut hoisted_declarations = Vec::new();
    for statement in functions {
        let loc = statement.loc;
        let Some(StmtData::Function(mut function)) = statement.data.map(|data| *data) else {
            unreachable!("block function classification changed");
        };
        let name = function
            .function
            .name
            .take()
            .expect("function declarations have names");
        let value = Expr::new(
            loc,
            ExprData::Function(FunctionExpr {
                function: function.function,
                ..FunctionExpr::default()
            }),
        );
        if let Some(&index) = declaration_index_by_reference.get(&name.reference) {
            lexical_declarations[index].value_or_nil = value;
            continue;
        }

        declaration_index_by_reference.insert(name.reference, lexical_declarations.len());
        if core.options.keep_names {
            let original_name = core.symbols
                [usize::try_from(name.reference.inner_index).expect("symbol index")]
            .original_name
            .clone();
            keep_name_statements.push(keep_block_function_name_statement(
                core,
                name.loc,
                name.reference,
                &original_name,
            ));
        }
        lexical_declarations.push(Decl {
            binding: identifier_binding(name.loc, name.reference),
            value_or_nil: value,
        });
        if let Some(&hoisted_reference) = core
            .hoisted_ref_for_sloppy_mode_block_fn
            .get(&name.reference)
        {
            core.record_declared_symbol(hoisted_reference);
            core.record_usage(name.reference);
            hoisted_declarations.push(Decl {
                binding: identifier_binding(name.loc, hoisted_reference),
                value_or_nil: Expr::new(
                    name.loc,
                    ExprData::Identifier(IdentifierExpr {
                        reference: name.reference,
                        ..IdentifierExpr::default()
                    }),
                ),
            });
        }
    }

    let lexical_kind = if core
        .options
        .unsupported_js_features
        .contains(JsFeature::CONST_AND_LET)
    {
        LocalKind::Var
    } else {
        LocalKind::Let
    };
    let lexical_loc = lexical_declarations
        .first()
        .map_or_else(Loc::default, |declaration| declaration.value_or_nil.loc);
    let mut lowered = Vec::with_capacity(2 + visited.len());
    if !lexical_declarations.is_empty() {
        lowered.push(Stmt::new(
            lexical_loc,
            StmtData::Local(LocalStmt {
                declarations: lexical_declarations,
                kind: lexical_kind,
                ..LocalStmt::default()
            }),
        ));
        lowered.append(&mut keep_name_statements);
    }
    if !hoisted_declarations.is_empty() {
        let hoisted_loc = hoisted_declarations[0].value_or_nil.loc;
        if should_relocate_vars_to_top_level(core) {
            let mut value = Expr::default();
            for declaration in hoisted_declarations {
                let Some(BindingData::Identifier(identifier)) = declaration.binding.data.as_deref()
                else {
                    unreachable!("hoisted block function binding must be an identifier");
                };
                core.relocated_top_level_vars
                    .push(crate::internal::ast::LocRef {
                        loc: declaration.binding.loc,
                        reference: identifier.reference,
                    });
                core.record_usage(identifier.reference);
                value = join_with_comma(
                    value,
                    assign(
                        convert_binding_to_expr(&declaration.binding, None),
                        declaration.value_or_nil,
                    ),
                );
            }
            lowered.push(Stmt::new(
                hoisted_loc,
                StmtData::Expr(ExprStmt {
                    value,
                    ..ExprStmt::default()
                }),
            ));
        } else {
            lowered.push(Stmt::new(
                hoisted_loc,
                StmtData::Local(LocalStmt {
                    declarations: hoisted_declarations,
                    kind: LocalKind::Var,
                    ..LocalStmt::default()
                }),
            ));
        }
    }
    lowered.extend(visited);
    lowered
}

fn lower_block_level_function_declarations(core: &mut ParserCore, statements: &mut Vec<Stmt>) {
    let Some(scope) = core.current_scope.as_ref() else {
        return;
    };
    let (kind, contains_direct_eval) = {
        let scope = scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (scope.kind, scope.contains_direct_eval)
    };
    if kind.stops_hoisting() {
        return;
    }

    let mut functions = Vec::new();
    let mut visited = Vec::with_capacity(statements.len());
    for statement in std::mem::take(statements) {
        let is_block_function = matches!(
            statement.data.as_deref(),
            Some(StmtData::Function(function))
                if function.function.name.is_some_and(|name| {
                    core.symbols
                        [usize::try_from(name.reference.inner_index).expect("symbol index")]
                    .kind
                        == SymbolKind::HoistedFunction
                })
        );
        if is_block_function {
            functions.push(statement);
        } else {
            visited.push(statement);
        }
    }
    if functions.is_empty() {
        *statements = visited;
        return;
    }

    if contains_direct_eval {
        preserve_block_functions_with_direct_eval(core, &functions);
        functions.extend(visited);
        *statements = functions;
        return;
    }

    *statements = lower_block_functions_to_declarations(core, functions, visited);
    if core.options.minify_syntax {
        inline_single_use_declarations(core, statements);
    }
}

fn visit_block(
    core: &mut ParserCore,
    loc: crate::internal::logger::Loc,
    block: &mut BlockStmt,
    resolve_identifiers: bool,
) {
    core.push_scope_for_visit_pass(ScopeKind::Block, loc);
    visit_statements(core, &mut block.statements, resolve_identifiers);
    lower_block_level_function_declarations(core, &mut block.statements);
    lower_nested_type_script_statements(core, &mut block.statements, None);
    core.pop_scope();
}

fn visit_function(core: &mut ParserCore, function: &mut Function, resolve_identifiers: bool) {
    let old_loop_depth = std::mem::take(&mut core.visit_loop_depth);
    let old_switch_depth = std::mem::take(&mut core.visit_switch_depth);
    let old_try_body_depth = std::mem::take(&mut core.visit_try_body_depth);
    let old_try_catch_loc = core.visit_try_catch_loc;
    let old_new_target_allowed = std::mem::replace(&mut core.visit_new_target_allowed, true);
    let old_is_async_generator = std::mem::replace(
        &mut core.visit_is_async_generator,
        function.is_async && function.is_generator,
    );
    let old_this_is_nested = std::mem::replace(&mut core.visit_this_is_nested, true);
    let old_is_outside_fn_or_arrow =
        std::mem::replace(&mut core.visit_is_outside_fn_or_arrow, false);
    if let Some(name) = function.name {
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
    let experimental_decorators =
        core.options.ts.config.experimental_decorators == crate::internal::config::MaybeBool::True;
    let argument_scope = core
        .current_scope
        .as_ref()
        .expect("function arguments scope")
        .clone();
    let mut hidden_argument_members = Vec::new();
    if experimental_decorators {
        let mut names = HashSet::new();
        let mut references = Vec::new();
        for argument in &function.args {
            collect_bindings(core, &argument.binding, &mut names, &mut references);
        }
        let references = references.into_iter().collect::<HashSet<_>>();
        let mut scope = argument_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for name in names {
            if scope
                .members
                .get(&name)
                .is_some_and(|member| references.contains(&member.reference))
                && let Some(member) = scope.members.remove(&name)
            {
                hidden_argument_members.push((name, member));
            }
        }
        drop(scope);
        for argument in &mut function.args {
            for decorator in &mut argument.decorators {
                visit_expr(core, &mut decorator.value, resolve_identifiers);
            }
        }
        argument_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .extend(hidden_argument_members);
    }
    for argument in &mut function.args {
        if !experimental_decorators {
            for decorator in &mut argument.decorators {
                visit_expr(core, &mut decorator.value, resolve_identifiers);
            }
        }
        record_binding_with_duplicates(
            core,
            &mut argument.binding,
            check_duplicates.then_some(&mut duplicate_args),
        );
        visit_binding_initializers(core, &mut argument.binding, resolve_identifiers);
        visit_expr(core, &mut argument.default_or_nil, resolve_identifiers);
    }
    core.push_scope_for_visit_pass(ScopeKind::FunctionBody, function.body.loc);
    preserve_directive_prologue(core, &mut function.body.block.statements);
    visit_statements(
        core,
        &mut function.body.block.statements,
        resolve_identifiers,
    );
    if core.options.keep_names && core.source.key_path.text != "<runtime>" {
        apply_keep_names_to_type_script_namespaces(core, &mut function.body.block.statements);
    }
    lower_nested_type_script_statements(core, &mut function.body.block.statements, None);
    if core.options.keep_names && core.source.key_path.text != "<runtime>" {
        apply_keep_names_to_statements(core, &mut function.body.block.statements);
    }
    if core.options.minify_syntax {
        optimize_implicit_jumps(
            core,
            &mut function.body.block.statements,
            ImplicitJumpKind::Return,
        );
        merge_adjacent_returns(core, &mut function.body.block.statements);
        cleanup_function_body_tail(core, &mut function.body.block.statements);
    }
    core.pop_scope();
    core.pop_scope();
    core.visit_loop_depth = old_loop_depth;
    core.visit_switch_depth = old_switch_depth;
    core.visit_try_body_depth = old_try_body_depth;
    core.visit_try_catch_loc = old_try_catch_loc;
    core.visit_new_target_allowed = old_new_target_allowed;
    core.visit_is_async_generator = old_is_async_generator;
    core.visit_this_is_nested = old_this_is_nested;
    core.visit_is_outside_fn_or_arrow = old_is_outside_fn_or_arrow;
}

fn preserve_directive_prologue(core: &ParserCore, statements: &mut [Stmt]) {
    for statement in statements {
        if matches!(statement.data.as_deref(), Some(StmtData::Comment(_))) {
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
        statement.data = Some(Box::new(StmtData::Directive(
            crate::internal::js_ast::DirectiveStmt {
                value: value.value.clone(),
                legacy_octal_loc: value.legacy_octal_loc,
            },
        )));
    }
}

fn mark_inlinable_function_declaration(core: &mut ParserCore, function: &Function) {
    if !core.options.minify_syntax
        || function.is_generator
        || function.is_async
        || function.has_rest_arg
    {
        return;
    }
    let Some(name) = function.name else {
        return;
    };
    let symbol =
        &mut core.symbols[usize::try_from(name.reference.inner_index).expect("symbol index")];
    if function
        .body
        .block
        .statements
        .iter()
        .all(|statement| statement.data.is_none())
        && function.args.iter().all(|argument| {
            matches!(
                argument.binding.data.as_deref(),
                Some(BindingData::Identifier(_))
            )
        })
    {
        symbol.flags |= SymbolFlags::IS_EMPTY_FUNCTION;
        return;
    }
    if let [argument] = function.args.as_slice()
        && argument.default_or_nil.data.is_none()
        && let Some(BindingData::Identifier(argument)) = argument.binding.data.as_deref()
        && let [statement] = function.body.block.statements.as_slice()
        && let Some(StmtData::Return(return_statement)) = statement.data.as_deref()
        && let Some(ExprData::Identifier(returned)) = return_statement.value_or_nil.data.as_deref()
        && argument.reference == returned.reference
    {
        symbol.flags |= SymbolFlags::IS_IDENTITY_FUNCTION;
    }
}

#[allow(clippy::too_many_lines)]
fn visit_class(
    core: &mut ParserCore,
    class: &mut Class,
    resolve_identifiers: bool,
    merge_inner_name: bool,
) -> Option<Ref> {
    let class_post_start = core.class_post_statements.len();
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
        let name_was_stored = ParserCore::is_stored_name_ref(name.reference);
        let text = if name_was_stored {
            String::from_utf8_lossy(core.load_name_from_ref(name.reference)).into_owned()
        } else {
            core.symbols[usize::try_from(name.reference.inner_index).expect("symbol index")]
                .original_name
                .clone()
        };
        let inner_reference = core.new_symbol(
            crate::internal::ast::SymbolKind::Const,
            if name_was_stored {
                text.clone()
            } else {
                format!("_{text}")
            },
        );
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
        if name_was_stored {
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
        let old_this_is_nested = std::mem::replace(&mut core.visit_this_is_nested, true);
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
        core.visit_this_is_nested = old_this_is_nested;
        core.current_scope
            .as_ref()
            .expect("class body scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forbid_arguments = false;
        if let Some(static_block) = &mut property.class_static_block {
            let old_loop_depth = std::mem::take(&mut core.visit_loop_depth);
            let old_switch_depth = std::mem::take(&mut core.visit_switch_depth);
            let old_try_body_depth = std::mem::take(&mut core.visit_try_body_depth);
            let old_try_catch_loc = core.visit_try_catch_loc;
            let old_new_target_allowed =
                std::mem::replace(&mut core.visit_new_target_allowed, true);
            let old_this_is_nested = std::mem::replace(&mut core.visit_this_is_nested, true);
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
            lower_nested_type_script_statements(core, &mut static_block.block.statements, None);
            core.pop_scope();
            core.visit_loop_depth = old_loop_depth;
            core.visit_switch_depth = old_switch_depth;
            core.visit_try_body_depth = old_try_body_depth;
            core.visit_try_catch_loc = old_try_catch_loc;
            core.visit_new_target_allowed = old_new_target_allowed;
            core.visit_this_is_nested = old_this_is_nested;
        }
    }
    preserve_type_script_omitted_computed_field_keys(core, class, resolve_identifiers);
    lower_standard_decorators(core, class, outer_class_name);
    let decorator_keys = prepare_type_script_computed_property_keys(core, class);
    lower_type_script_experimental_decorators(core, class, outer_class_name, &decorator_keys);
    lower_type_script_static_field_assignments(core, class, outer_class_name, class_post_start);
    lower_type_script_class_field_assignments(core, class);
    if let Some(constructor_index) = class_constructor_index(class) {
        let parameter_field_count = class.properties[constructor_index]
            .value_or_nil
            .data
            .as_deref()
            .and_then(|data| match data {
                ExprData::Function(function) => Some(
                    function
                        .function
                        .args
                        .iter()
                        .filter(|argument| argument.is_typescript_ctor_field)
                        .count(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        if constructor_index != 0 && parameter_field_count != 0 {
            let generated_field_count = if class.use_define_for_class_fields {
                parameter_field_count
            } else {
                0
            };
            let end = (constructor_index + 1 + generated_field_count).min(class.properties.len());
            let constructor_and_fields: Vec<_> =
                class.properties.drain(constructor_index..end).collect();
            class.properties.splice(0..0, constructor_and_fields);
        }
        if let Some(ExprData::Function(function)) =
            class.properties[0].value_or_nil.data.as_deref_mut()
        {
            for argument in &mut function.function.args {
                argument.is_typescript_ctor_field = false;
            }
        }
    }
    core.pop_scope();
    core.pop_scope();
    let used_inner_name = inner_class_name.filter(|inner| {
        core.symbols[usize::try_from(inner.inner_index).expect("symbol index")].use_count_estimate
            != 0
    });
    if core.options.minify_syntax && outer_class_name.is_none() && used_inner_name.is_none() {
        class.name = None;
    }
    let merge_inner_name = merge_inner_name
        || core.options.ts.config.experimental_decorators
            == crate::internal::config::MaybeBool::True
        || class.should_lower_standard_decorators;
    if merge_inner_name && let (Some(inner), Some(outer)) = (used_inner_name, outer_class_name) {
        core.merge_symbols(inner, outer);
        None
    } else {
        used_inner_name
    }
}

fn preserve_type_script_omitted_computed_field_keys(
    core: &mut ParserCore,
    class: &mut Class,
    resolve_identifiers: bool,
) {
    if !core.options.ts.parse
        || class.use_define_for_class_fields
        || core.options.ts.config.experimental_decorators
            == crate::internal::config::MaybeBool::True
    {
        return;
    }

    let mut pending = Expr::default();
    let mut last_computed_index: Option<usize> = None;
    let mut last_computed_prefix = Expr::default();
    let mut retained: Vec<Property> = Vec::with_capacity(class.properties.len());
    for mut property in std::mem::take(&mut class.properties) {
        let omit = property.kind == PropertyKind::Field
            && property.flags.contains(PropertyFlags::IS_COMPUTED)
            && property.initializer_or_nil.data.is_none()
            && property.value_or_nil.data.is_none()
            && property.decorators.is_empty()
            && !matches!(
                property.key.data.as_deref(),
                Some(ExprData::PrivateIdentifier(_))
            );
        if omit {
            let mut key = std::mem::take(&mut property.key);
            if !resolve_identifiers {
                visit_expr(core, &mut key, true);
            }
            pending = join_with_comma(pending, key);
            continue;
        }
        if property.flags.contains(PropertyFlags::IS_COMPUTED)
            && property.kind.is_method_definition()
        {
            if let Some(index) = last_computed_index
                && last_computed_prefix.data.is_some()
            {
                let previous = &mut retained[index];
                previous.key = join_with_comma(
                    std::mem::take(&mut last_computed_prefix),
                    std::mem::take(&mut previous.key),
                );
            }
            last_computed_index = Some(retained.len());
            last_computed_prefix = std::mem::take(&mut pending);
        }
        retained.push(property);
    }
    class.properties = retained;

    if pending.data.is_none() {
        if let Some(index) = last_computed_index
            && last_computed_prefix.data.is_some()
        {
            let property = &mut class.properties[index];
            property.key = join_with_comma(last_computed_prefix, std::mem::take(&mut property.key));
        }
        return;
    }
    if let Some(index) = last_computed_index {
        let property = &mut class.properties[index];
        let loc = property.key.loc;
        let reference = generate_class_computed_key_temp(core, loc);
        let original_key = std::mem::take(&mut property.key);
        property.key = join_with_comma(
            join_with_comma(
                join_with_comma(
                    last_computed_prefix,
                    assign(computed_key_temp(core, loc, reference), original_key),
                ),
                pending,
            ),
            computed_key_temp(core, loc, reference),
        );
    } else if class.extends_or_nil.data.is_some() {
        let loc = class.extends_or_nil.loc;
        let reference = generate_class_computed_key_temp(core, loc);
        let extends = std::mem::take(&mut class.extends_or_nil);
        class.extends_or_nil = join_with_comma(
            join_with_comma(
                assign(computed_key_temp(core, loc, reference), extends),
                pending,
            ),
            computed_key_temp(core, loc, reference),
        );
    } else {
        core.class_pre_statements.push(Stmt::new(
            class.body_loc,
            StmtData::Expr(ExprStmt {
                value: pending,
                ..ExprStmt::default()
            }),
        ));
    }
}

fn generate_class_computed_key_temp(core: &mut ParserCore, loc: Loc) -> Ref {
    let mut enclosing_scope = core.current_scope.clone();
    while let Some(scope) = enclosing_scope.clone() {
        let (kind, parent) = {
            let scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                scope.kind,
                scope.parent.as_ref().and_then(std::sync::Weak::upgrade),
            )
        };
        if !matches!(kind, ScopeKind::ClassBody | ScopeKind::ClassName) {
            break;
        }
        enclosing_scope = parent;
    }
    let is_top_level = enclosing_scope.as_ref().is_some_and(|scope| {
        core.module_scope
            .as_ref()
            .is_some_and(|module| std::sync::Arc::ptr_eq(scope, module))
    });
    if is_top_level {
        return core.generate_top_level_temp_ref();
    }

    let reference = core.new_symbol(SymbolKind::Other, "_a");
    if let Some(scope) = enclosing_scope {
        scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generated
            .push(reference);
    }
    core.record_declared_symbol(reference);
    core.class_pre_statements.push(Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations: vec![Decl {
                binding: identifier_binding(loc, reference),
                ..Decl::default()
            }],
            kind: LocalKind::Var,
            ..LocalStmt::default()
        }),
    ));
    reference
}

fn decorator_target(loc: Loc, reference: Ref) -> Expr {
    Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }),
    )
}

fn decorator_array(loc: Loc, decorators: Vec<crate::internal::js_ast::Decorator>) -> Expr {
    Expr::new(
        loc,
        ExprData::Array(crate::internal::js_ast::ArrayExpr {
            items: decorators
                .into_iter()
                .map(|decorator| decorator.value)
                .collect(),
            ..crate::internal::js_ast::ArrayExpr::default()
        }),
    )
}

fn decorator_statement(loc: Loc, value: Expr) -> Stmt {
    Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value,
            ..ExprStmt::default()
        }),
    )
}

fn standard_decorator_temp(core: &mut ParserCore, name: String) -> Ref {
    core.generate_named_top_level_temp_ref(name)
}

fn standard_decorator_ref(core: &mut ParserCore, loc: Loc, reference: Ref) -> Expr {
    core.record_usage(reference);
    decorator_target(loc, reference)
}

fn standard_decorator_string(loc: Loc, value: &str) -> Expr {
    Expr::new(
        loc,
        ExprData::String(StringExpr {
            value: string_to_utf16(value.as_bytes()),
            ..StringExpr::default()
        }),
    )
}

fn standard_decorator_array(loc: Loc, decorators: Vec<crate::internal::js_ast::Decorator>) -> Expr {
    Expr::new(
        loc,
        ExprData::Array(crate::internal::js_ast::ArrayExpr {
            items: decorators
                .into_iter()
                .map(|decorator| decorator.value)
                .collect(),
            is_single_line: true,
            ..crate::internal::js_ast::ArrayExpr::default()
        }),
    )
}

fn standard_decorator_this(loc: Loc) -> Expr {
    Expr::new(loc, ExprData::This)
}

fn insert_standard_decorator_initializers(
    core: &mut ParserCore,
    class: &mut Class,
    statements: Vec<Stmt>,
) {
    if statements.is_empty() {
        return;
    }
    if let Some(index) = class_constructor_index(class) {
        if let Some(ExprData::Function(function)) =
            class.properties[index].value_or_nil.data.as_deref_mut()
        {
            if class.extends_or_nil.data.is_some() {
                insert_parameter_fields_after_super(
                    &mut function.function.body.block.statements,
                    &statements,
                );
            } else {
                function
                    .function
                    .body
                    .block
                    .statements
                    .splice(0..0, statements);
            }
        }
    } else {
        append_class_field_constructor(
            core,
            class,
            statements,
            class.extends_or_nil.data.is_some(),
        );
    }
}

fn lower_standard_decorators(
    core: &mut ParserCore,
    class: &mut Class,
    outer_class_name: Option<Ref>,
) {
    if !class.should_lower_standard_decorators {
        return;
    }
    let Some(class_ref) = outer_class_name else {
        return;
    };
    let loc = class.class_keyword.loc;
    let class_name = symbol_name(core, class_ref);
    let context_ref = standard_decorator_temp(core, "_init".into());
    let mut pre = Expr::default();
    let mut post_storage = Vec::new();
    let mut post_decorators = Vec::new();
    let mut instance_initializers = Vec::new();
    let mut retained = Vec::with_capacity(class.properties.len());
    let mut property_decorator_refs = Vec::new();
    let mut storage_refs = Vec::new();
    let mut decorated_field_index = 0_u32;
    let mut has_instance_method_decorators = false;

    let class_decorator_ref = if class.decorators.is_empty() {
        None
    } else {
        let at_loc = class.decorators[0].at_loc;
        let reference = standard_decorator_temp(core, format!("_{class_name}_decorators"));
        let decorators = standard_decorator_array(at_loc, std::mem::take(&mut class.decorators));
        pre = join_with_comma(
            pre,
            assign(standard_decorator_ref(core, at_loc, reference), decorators),
        );
        Some(reference)
    };

    for mut property in std::mem::take(&mut class.properties) {
        if property.decorators.is_empty() {
            retained.push(property);
            continue;
        }
        let property_loc = property.loc;
        let property_name = match property.key.data.as_deref() {
            Some(ExprData::String(value)) => String::from_utf16_lossy(&value.value),
            _ => {
                retained.push(property);
                continue;
            }
        };
        let decorator_ref = standard_decorator_temp(core, format!("_{property_name}_dec"));
        property_decorator_refs.push(decorator_ref);
        let decorators = standard_decorator_array(
            property.decorators[0].at_loc,
            std::mem::take(&mut property.decorators),
        );
        pre = join_with_comma(
            pre,
            assign(
                standard_decorator_ref(core, property_loc, decorator_ref),
                decorators,
            ),
        );

        let mut decorate_args = vec![
            standard_decorator_ref(core, property_loc, context_ref),
            Expr::new(
                property_loc,
                ExprData::Number(match property.kind {
                    PropertyKind::Field => 5.0,
                    PropertyKind::AutoAccessor => 4.0,
                    _ => 1.0,
                }),
            ),
            standard_decorator_string(property_loc, &property_name),
            standard_decorator_ref(core, property_loc, decorator_ref),
            standard_decorator_ref(core, property_loc, class_ref),
        ];

        match property.kind {
            PropertyKind::Method | PropertyKind::Getter | PropertyKind::Setter => {
                if !property.flags.contains(PropertyFlags::IS_STATIC) {
                    has_instance_method_decorators = true;
                }
                retained.push(property);
            }
            PropertyKind::Field | PropertyKind::AutoAccessor
                if !property.flags.contains(PropertyFlags::IS_STATIC) =>
            {
                let initializer_flags = 8.0 + f64::from(decorated_field_index * 4);
                decorated_field_index += 1;
                let mut initializer_args = vec![
                    standard_decorator_ref(core, property_loc, context_ref),
                    Expr::new(property_loc, ExprData::Number(initializer_flags)),
                    standard_decorator_this(property_loc),
                ];
                let initializer = if property.initializer_or_nil.data.is_some() {
                    std::mem::take(&mut property.initializer_or_nil)
                } else if property.value_or_nil.data.is_some() {
                    std::mem::take(&mut property.value_or_nil)
                } else {
                    Expr::default()
                };
                if initializer.data.is_some() {
                    initializer_args.push(initializer);
                }
                let initialized =
                    core.call_runtime(property_loc, "__runInitializers", initializer_args);
                let first = if property.kind == PropertyKind::AutoAccessor {
                    let storage_ref = standard_decorator_temp(core, format!("_{property_name}"));
                    storage_refs.push(storage_ref);
                    decorate_args.push(standard_decorator_ref(core, property_loc, storage_ref));
                    let weak_map = core.find_symbol(property_loc, "WeakMap").reference;
                    core.record_usage(weak_map);
                    post_storage.push(decorator_statement(
                        property_loc,
                        assign(
                            standard_decorator_ref(core, property_loc, storage_ref),
                            Expr::new(
                                property_loc,
                                ExprData::New(NewExpr {
                                    target: decorator_target(property_loc, weak_map),
                                    ..NewExpr::default()
                                }),
                            ),
                        ),
                    ));
                    let storage = standard_decorator_ref(core, property_loc, storage_ref);
                    core.call_runtime(
                        property_loc,
                        "__privateAdd",
                        vec![standard_decorator_this(property_loc), storage, initialized],
                    )
                } else {
                    let target = class_field_assignment_target(&property)
                        .expect("decorated public field has an assignment target");
                    assign(target, initialized)
                };
                let context = standard_decorator_ref(core, property_loc, context_ref);
                let extra = core.call_runtime(
                    property_loc,
                    "__runInitializers",
                    vec![
                        context,
                        Expr::new(property_loc, ExprData::Number(initializer_flags + 3.0)),
                        standard_decorator_this(property_loc),
                    ],
                );
                instance_initializers.push(decorator_statement(
                    property_loc,
                    join_with_comma(first, extra),
                ));
            }
            _ => retained.push(property),
        }
        post_decorators.push(decorator_statement(
            property_loc,
            core.call_runtime(property_loc, "__decorateElement", decorate_args),
        ));
    }
    class.properties = retained;

    if has_instance_method_decorators {
        let context = standard_decorator_ref(core, loc, context_ref);
        let initializer = core.call_runtime(
            loc,
            "__runInitializers",
            vec![
                context,
                Expr::new(loc, ExprData::Number(5.0)),
                standard_decorator_this(loc),
            ],
        );
        instance_initializers.insert(0, decorator_statement(loc, initializer));
    }
    insert_standard_decorator_initializers(core, class, instance_initializers);

    let mut declarations = Vec::new();
    if let Some(reference) = class_decorator_ref {
        declarations.push(reference);
    }
    declarations.extend(property_decorator_refs.into_iter().rev());
    declarations.push(context_ref);
    declarations.extend(storage_refs);
    core.class_pre_statements.push(Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations: declarations
                .into_iter()
                .map(|reference| Decl {
                    binding: identifier_binding(loc, reference),
                    ..Decl::default()
                })
                .collect(),
            kind: LocalKind::Var,
            ..LocalStmt::default()
        }),
    ));
    if pre.data.is_some() {
        core.class_pre_statements
            .push(decorator_statement(loc, pre));
    }
    let context = standard_decorator_ref(core, loc, context_ref);
    let start = core.call_runtime(
        loc,
        "__decoratorStart",
        vec![Expr::new(loc, ExprData::Null)],
    );
    let start = decorator_statement(loc, assign(context, start));
    core.class_post_statements.push(start);
    core.class_post_statements.extend(post_storage);
    core.class_post_statements.extend(post_decorators);
    if let Some(decorator_ref) = class_decorator_ref {
        let context = standard_decorator_ref(core, loc, context_ref);
        let decorators = standard_decorator_ref(core, loc, decorator_ref);
        let target = standard_decorator_ref(core, loc, class_ref);
        let decorated = core.call_runtime(
            loc,
            "__decorateElement",
            vec![
                context,
                Expr::new(loc, ExprData::Number(0.0)),
                standard_decorator_string(loc, &class_name),
                decorators,
                target,
            ],
        );
        let target = standard_decorator_ref(core, loc, class_ref);
        let statement = decorator_statement(loc, assign(target, decorated));
        core.class_post_statements.push(statement);
        let initialized_class = standard_decorator_ref(core, loc, class_ref);
        let context = standard_decorator_ref(core, loc, context_ref);
        let initialized = core.call_runtime(
            loc,
            "__runInitializers",
            vec![
                context,
                Expr::new(loc, ExprData::Number(1.0)),
                initialized_class,
            ],
        );
        core.class_post_statements
            .push(decorator_statement(loc, initialized));
    } else {
        let metadata_target = standard_decorator_ref(core, loc, class_ref);
        let context = standard_decorator_ref(core, loc, context_ref);
        let metadata =
            core.call_runtime(loc, "__decoratorMetadata", vec![context, metadata_target]);
        core.class_post_statements
            .push(decorator_statement(loc, metadata));
    }
}

fn computed_key_temp(core: &mut ParserCore, loc: Loc, reference: Ref) -> Expr {
    core.record_usage(reference);
    Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }),
    )
}

fn prepare_type_script_computed_property_keys(
    core: &mut ParserCore,
    class: &mut Class,
) -> Vec<Option<Expr>> {
    if core.options.ts.config.experimental_decorators != crate::internal::config::MaybeBool::True {
        return vec![None; class.properties.len()];
    }

    let needs_temp = class
        .properties
        .iter()
        .map(|property| {
            property.flags.contains(PropertyFlags::IS_COMPUTED)
                && if class.use_define_for_class_fields {
                    !property.decorators.is_empty()
                } else {
                    !property.decorators.is_empty()
                        || property.initializer_or_nil.data.is_some()
                        || property.value_or_nil.data.is_some()
                        || property.kind.is_method_definition()
                        || property.kind == PropertyKind::DeclareOrAbstract
                }
        })
        .collect::<Vec<_>>();
    let temp_count = needs_temp.iter().filter(|&&value| value).count();
    if temp_count == 0 {
        return vec![None; class.properties.len()];
    }
    let mut enclosing_scope = core.current_scope.clone();
    while let Some(scope) = enclosing_scope.clone() {
        let (kind, parent) = {
            let scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                scope.kind,
                scope.parent.as_ref().and_then(std::sync::Weak::upgrade),
            )
        };
        if !matches!(kind, ScopeKind::ClassBody | ScopeKind::ClassName) {
            break;
        }
        enclosing_scope = parent;
    }
    let is_top_level = enclosing_scope.as_ref().is_some_and(|scope| {
        core.module_scope
            .as_ref()
            .is_some_and(|module| std::sync::Arc::ptr_eq(scope, module))
    });
    let mut temp_refs = if is_top_level {
        (0..temp_count)
            .map(|_| core.generate_top_level_temp_ref())
            .collect::<Vec<_>>()
    } else {
        let references = (0..temp_count)
            .map(|index| {
                let suffix =
                    char::from_u32(u32::from(b'a') + u32::try_from(index).unwrap_or(25).min(25))
                        .unwrap_or('z');
                core.new_symbol(SymbolKind::Other, format!("_{suffix}"))
            })
            .collect::<Vec<_>>();
        if let Some(scope) = enclosing_scope {
            scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generated
                .extend(references.iter().copied());
        }
        let loc = class.body_loc;
        for &reference in &references {
            core.record_declared_symbol(reference);
        }
        core.class_pre_statements.push(Stmt::new(
            loc,
            StmtData::Local(LocalStmt {
                declarations: references
                    .iter()
                    .map(|&reference| Decl {
                        binding: identifier_binding(loc, reference),
                        ..Decl::default()
                    })
                    .collect(),
                kind: LocalKind::Var,
                ..LocalStmt::default()
            }),
        ));
        references
    };
    temp_refs.reverse();
    let mut next_temp = 0usize;
    let mut decorator_keys = vec![None; class.properties.len()];
    if class.use_define_for_class_fields {
        for (index, property) in class.properties.iter_mut().enumerate() {
            if !needs_temp[index] {
                continue;
            }
            let reference = temp_refs[next_temp];
            next_temp += 1;
            let loc = property.key.loc;
            property.key = assign(
                computed_key_temp(core, loc, reference),
                std::mem::take(&mut property.key),
            );
            decorator_keys[index] = Some(computed_key_temp(core, loc, reference));
        }
        return decorator_keys;
    }
    let mut pending = Expr::default();
    let mut last_computed_method: Option<(usize, Ref)> = None;

    for (index, property) in class.properties.iter_mut().enumerate() {
        if !property.flags.contains(PropertyFlags::IS_COMPUTED) {
            continue;
        }
        let loc = property.key.loc;
        let original_key = std::mem::take(&mut property.key);
        let evaluation = if needs_temp[index] {
            let reference = temp_refs[next_temp];
            next_temp += 1;
            let assignment = assign(computed_key_temp(core, loc, reference), original_key);
            property.key = computed_key_temp(core, loc, reference);
            decorator_keys[index] = Some(computed_key_temp(core, loc, reference));
            if property.kind.is_method_definition() {
                last_computed_method = Some((index, reference));
            }
            assignment
        } else {
            property.key = original_key.clone();
            original_key
        };
        pending = join_with_comma(pending, evaluation);

        if property.kind.is_method_definition() {
            property.key = std::mem::take(&mut pending);
        }
    }

    if pending.data.is_some()
        && let Some((method_index, method_ref)) = last_computed_method
    {
        let method = &mut class.properties[method_index];
        let before = std::mem::take(&mut method.key);
        method.key = join_with_comma(
            join_with_comma(before, pending),
            computed_key_temp(core, method.loc, method_ref),
        );
    } else if pending.data.is_some() {
        core.class_pre_statements.push(Stmt::new(
            class.body_loc,
            StmtData::Expr(ExprStmt {
                value: pending,
                ..ExprStmt::default()
            }),
        ));
    }

    decorator_keys
}

fn lower_type_script_experimental_decorators(
    core: &mut ParserCore,
    class: &mut Class,
    outer_class_name: Option<Ref>,
    decorator_keys: &[Option<Expr>],
) {
    if core.options.ts.config.experimental_decorators != crate::internal::config::MaybeBool::True {
        return;
    }
    let Some(class_ref) = outer_class_name else {
        return;
    };
    let mut instance_decorators = Vec::new();
    let mut static_decorators = Vec::new();
    for (property_index, property) in class.properties.iter_mut().enumerate() {
        if property.kind.is_method_definition()
            && let Some(ExprData::Function(function)) = property.value_or_nil.data.as_deref_mut()
        {
            let is_constructor = matches!(
                property.key.data.as_deref(),
                Some(ExprData::String(key))
                    if String::from_utf16_lossy(&key.value) == "constructor"
            );
            for (index, argument) in function.function.args.iter_mut().enumerate() {
                for decorator in std::mem::take(&mut argument.decorators) {
                    let loc = decorator.value.loc;
                    let value = core.call_runtime(
                        loc,
                        "__decorateParam",
                        vec![
                            Expr::new(loc, ExprData::Number(index as f64)),
                            decorator.value,
                        ],
                    );
                    let decorator = crate::internal::js_ast::Decorator {
                        value,
                        at_loc: decorator.at_loc,
                        omit_newline_after: decorator.omit_newline_after,
                    };
                    if is_constructor {
                        class.decorators.push(decorator);
                    } else {
                        property.decorators.push(decorator);
                    }
                }
            }
        }
        if property.decorators.is_empty() {
            continue;
        }
        let loc = property.key.loc;
        let decorators = decorator_array(loc, std::mem::take(&mut property.decorators));
        core.record_usage(class_ref);
        let class_target = decorator_target(loc, class_ref);
        let target = if property.flags.contains(PropertyFlags::IS_STATIC) {
            class_target
        } else {
            Expr::new(
                loc,
                ExprData::Dot(DotExpr {
                    target: class_target,
                    name: "prototype".into(),
                    name_loc: loc,
                    ..DotExpr::default()
                }),
            )
        };
        let descriptor_kind = if matches!(
            property.kind,
            PropertyKind::Field | PropertyKind::DeclareOrAbstract
        ) {
            2.0
        } else {
            1.0
        };
        let key = decorator_keys
            .get(property_index)
            .and_then(Option::as_ref)
            .unwrap_or(&property.key)
            .clone();
        let call = core.call_runtime(
            loc,
            "__decorateClass",
            vec![
                decorators,
                target,
                key,
                Expr::new(loc, ExprData::Number(descriptor_kind)),
            ],
        );
        let statement = decorator_statement(loc, call);
        if property.flags.contains(PropertyFlags::IS_STATIC) {
            static_decorators.push(statement);
        } else {
            instance_decorators.push(statement);
        }
    }
    core.class_post_statements.extend(instance_decorators);
    core.class_post_statements.extend(static_decorators);
    if !class.decorators.is_empty() {
        let loc = class.decorators[0].at_loc;
        let decorators = decorator_array(loc, std::mem::take(&mut class.decorators));
        core.record_usage(class_ref);
        core.record_usage(class_ref);
        let call = core.call_runtime(
            loc,
            "__decorateClass",
            vec![decorators, decorator_target(loc, class_ref)],
        );
        core.class_post_statements.push(decorator_statement(
            loc,
            assign(decorator_target(loc, class_ref), call),
        ));
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

    let mut assignments = if lower_private_fields {
        rewrite_type_script_auto_accessors(core, class)
    } else {
        Vec::new()
    };
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
            return true;
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
            let parameter_field_count = function
                .function
                .args
                .iter()
                .filter(|argument| argument.is_typescript_ctor_field)
                .count();
            function
                .function
                .body
                .block
                .statements
                .splice(parameter_field_count..parameter_field_count, assignments);
        }
        if core.options.ts.config.experimental_decorators
            == crate::internal::config::MaybeBool::True
            && constructor_index != 0
        {
            let constructor = class.properties.remove(constructor_index);
            class.properties.insert(0, constructor);
        }
    } else {
        append_class_field_constructor(core, class, assignments, is_derived);
    }
}

fn rewrite_type_script_auto_accessors(core: &mut ParserCore, class: &mut Class) -> Vec<Stmt> {
    let mut assignments = Vec::new();
    let mut properties = Vec::with_capacity(class.properties.len());
    for mut property in std::mem::take(&mut class.properties) {
        if property.kind != PropertyKind::AutoAccessor
            || property.flags.contains(PropertyFlags::IS_STATIC)
            || !property.decorators.is_empty()
        {
            properties.push(property);
            continue;
        }

        let storage_name = match property.key.data.as_deref() {
            Some(ExprData::String(name)) => String::from_utf16_lossy(&name.value),
            Some(ExprData::PrivateIdentifier(private)) => symbol_name(core, private.reference),
            _ => {
                properties.push(property);
                continue;
            }
        };
        let loc = property.loc;
        let storage_ref = core.generate_auto_accessor_storage_ref(&storage_name);
        let storage = || {
            Expr::new(
                loc,
                ExprData::Identifier(IdentifierExpr {
                    reference: storage_ref,
                    ..IdentifierExpr::default()
                }),
            )
        };
        let this = || Expr::new(loc, ExprData::This);
        let initializer = if property.initializer_or_nil.data.is_some() {
            std::mem::take(&mut property.initializer_or_nil)
        } else if property.value_or_nil.data.is_some() {
            std::mem::take(&mut property.value_or_nil)
        } else {
            Expr::new(loc, ExprData::Undefined)
        };

        core.record_usage(storage_ref);
        assignments.push(Stmt::new(
            loc,
            StmtData::Expr(ExprStmt {
                value: core.call_runtime(loc, "__privateAdd", vec![this(), storage(), initializer]),
                ..ExprStmt::default()
            }),
        ));

        core.record_usage(storage_ref);
        let getter = Property {
            key: property.key.clone(),
            value_or_nil: Expr::new(
                loc,
                ExprData::Function(FunctionExpr {
                    function: Function {
                        body: FunctionBody {
                            loc,
                            block: BlockStmt {
                                statements: vec![Stmt::new(
                                    loc,
                                    StmtData::Return(ReturnStmt {
                                        value_or_nil: core.call_runtime(
                                            loc,
                                            "__privateGet",
                                            vec![this(), storage()],
                                        ),
                                    }),
                                )],
                                ..BlockStmt::default()
                            },
                        },
                        ..Function::default()
                    },
                    ..FunctionExpr::default()
                }),
            ),
            loc,
            kind: PropertyKind::Getter,
            flags: property.flags,
            ..Property::default()
        };

        let argument_ref = core.new_symbol(SymbolKind::Other, "_");
        core.record_usage(argument_ref);
        core.record_usage(storage_ref);
        let argument = Expr::new(
            loc,
            ExprData::Identifier(IdentifierExpr {
                reference: argument_ref,
                ..IdentifierExpr::default()
            }),
        );
        let setter = Property {
            key: property.key,
            value_or_nil: Expr::new(
                loc,
                ExprData::Function(FunctionExpr {
                    function: Function {
                        args: vec![Arg {
                            binding: identifier_binding(loc, argument_ref),
                            ..Arg::default()
                        }],
                        body: FunctionBody {
                            loc,
                            block: BlockStmt {
                                statements: vec![Stmt::new(
                                    loc,
                                    StmtData::Expr(ExprStmt {
                                        value: core.call_runtime(
                                            loc,
                                            "__privateSet",
                                            vec![this(), storage(), argument],
                                        ),
                                        ..ExprStmt::default()
                                    }),
                                )],
                                ..BlockStmt::default()
                            },
                        },
                        ..Function::default()
                    },
                    ..FunctionExpr::default()
                }),
            ),
            loc,
            kind: PropertyKind::Setter,
            flags: property.flags,
            ..Property::default()
        };
        properties.push(getter);
        properties.push(setter);

        let weak_map_ref = core.find_symbol(loc, "WeakMap").reference;
        core.record_usage(weak_map_ref);
        core.record_usage(storage_ref);
        core.class_post_statements.push(Stmt::new(
            loc,
            StmtData::Expr(ExprStmt {
                value: assign(
                    storage(),
                    Expr::new(
                        loc,
                        ExprData::New(NewExpr {
                            target: Expr::new(
                                loc,
                                ExprData::Identifier(IdentifierExpr {
                                    reference: weak_map_ref,
                                    ..IdentifierExpr::default()
                                }),
                            ),
                            ..NewExpr::default()
                        }),
                    ),
                ),
                ..ExprStmt::default()
            }),
        ));
    }
    class.properties = properties;
    assignments
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
    class.properties.insert(
        0,
        Property {
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
        },
    );
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
    if property.flags.contains(PropertyFlags::IS_COMPUTED) {
        return Some(Expr::new(
            property.loc,
            ExprData::Index(crate::internal::js_ast::IndexExpr {
                target: Expr::new(property.loc, ExprData::This),
                index: property.key.clone(),
                ..crate::internal::js_ast::IndexExpr::default()
            }),
        ));
    }
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

fn lower_type_script_static_field_assignments(
    core: &mut ParserCore,
    class: &mut Class,
    class_ref: Option<Ref>,
    insert_at: usize,
) {
    if class.use_define_for_class_fields {
        return;
    }
    class.properties.retain(|property| {
        if property.kind == PropertyKind::DeclareOrAbstract {
            return false;
        }
        if property.kind != PropertyKind::Field
            || property.initializer_or_nil.data.is_some()
            || property.value_or_nil.data.is_some()
        {
            return true;
        }
        (property.flags.contains(PropertyFlags::IS_COMPUTED)
            && core.options.ts.config.experimental_decorators
                != crate::internal::config::MaybeBool::True)
            || matches!(
                property.key.data.as_deref(),
                Some(ExprData::PrivateIdentifier(_))
            )
    });
    if core.options.mode != crate::internal::config::Mode::Bundle
        || core.options.ts.config.experimental_decorators
            != crate::internal::config::MaybeBool::True
    {
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
                class_static_block: Some(Box::new(crate::internal::js_ast::ClassStaticBlock {
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
        return;
    }
    let mut assignments = Vec::new();
    class.properties.retain_mut(|property| {
        if property.kind == PropertyKind::DeclareOrAbstract {
            return false;
        }
        if property.kind != PropertyKind::Field {
            return true;
        }
        if !property.flags.contains(PropertyFlags::IS_STATIC) {
            let initializer_is_missing =
                property.initializer_or_nil.data.is_none() && property.value_or_nil.data.is_none();
            let is_private = matches!(
                property.key.data.as_deref(),
                Some(ExprData::PrivateIdentifier(_))
            );
            return !initializer_is_missing || is_private;
        }
        if matches!(
            property.key.data.as_deref(),
            Some(ExprData::PrivateIdentifier(_))
        ) {
            return true;
        }

        let initializer = if property.initializer_or_nil.data.is_some() {
            std::mem::take(&mut property.initializer_or_nil)
        } else if property.value_or_nil.data.is_some() {
            std::mem::take(&mut property.value_or_nil)
        } else {
            return false;
        };
        let Some(class_ref) = class_ref else {
            return true;
        };
        let class_target = computed_key_temp(core, property.loc, class_ref);
        let target = if property.flags.contains(PropertyFlags::IS_COMPUTED) {
            Expr::new(
                property.loc,
                ExprData::Index(crate::internal::js_ast::IndexExpr {
                    target: class_target,
                    index: property.key.clone(),
                    ..crate::internal::js_ast::IndexExpr::default()
                }),
            )
        } else if let Some(ExprData::String(key)) = property.key.data.as_deref() {
            Expr::new(
                property.loc,
                ExprData::Dot(DotExpr {
                    target: class_target,
                    name: String::from_utf16_lossy(&key.value),
                    name_loc: property.key.loc,
                    ..DotExpr::default()
                }),
            )
        } else {
            Expr::new(
                property.loc,
                ExprData::Index(crate::internal::js_ast::IndexExpr {
                    target: class_target,
                    index: property.key.clone(),
                    ..crate::internal::js_ast::IndexExpr::default()
                }),
            )
        };
        assignments.push(class_field_assignment(property.loc, target, initializer));
        false
    });
    if !assignments.is_empty() {
        core.class_post_statements
            .splice(insert_at..insert_at, assignments);
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
    }
    class.properties.splice(
        constructor_index + 1..constructor_index + 1,
        field_properties,
    );
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
    stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || {
        visit_expr_with_target(core, expression, resolve_identifiers, AssignTarget::None);
    });
}

fn maybe_rewrite_import_namespace_property(
    core: &mut ParserCore,
    target: &Expr,
    name: &str,
    name_loc: Loc,
    prefer_quoted_key: bool,
) -> Option<ExprData> {
    if core.options.mode != crate::internal::config::Mode::Bundle {
        return None;
    }
    let Some(ExprData::Identifier(identifier)) = target.data.as_deref() else {
        return None;
    };
    let namespace_ref = identifier.reference;
    let (existing, import_record_index) = {
        let items = core.import_items_for_namespace.get(&namespace_ref)?;
        (items.entries.get(name).copied(), items.import_record_index)
    };
    let item = if let Some(item) = existing {
        item
    } else {
        let reference = core.new_symbol(SymbolKind::Import, name);
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
            .import_item_status = crate::internal::ast::ImportItemStatus::Generated;
        core.module_scope
            .as_ref()
            .expect("generated namespace import item requires a module scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generated
            .push(reference);
        let item = crate::internal::ast::LocRef {
            loc: name_loc,
            reference,
        };
        core.import_items_for_namespace
            .get_mut(&namespace_ref)
            .expect("namespace import items must still exist")
            .entries
            .insert(name.into(), item);
        core.is_import_item.insert(reference);
        core.generated_named_imports.insert(
            reference,
            crate::internal::js_ast::NamedImport {
                alias: name.into(),
                alias_loc: name_loc,
                namespace_ref,
                import_record_index,
                ..crate::internal::js_ast::NamedImport::default()
            },
        );
        item
    };
    core.ignore_usage(namespace_ref);
    core.record_usage(item.reference);
    Some(ExprData::ImportIdentifier(
        crate::internal::js_ast::ImportIdentifierExpr {
            reference: item.reference,
            prefer_quoted_key,
            was_originally_identifier: false,
        },
    ))
}

fn iife_can_be_removed_if_unused(core: &ParserCore, args: &[Arg], body: &FunctionBody) -> bool {
    let helpers = make_helper_context(|reference| {
        core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
            == SymbolKind::Unbound
    });
    for argument in args {
        if argument.default_or_nil.data.is_some()
            && !helpers.expr_can_be_removed_if_unused(&argument.default_or_nil)
        {
            return false;
        }
        if !matches!(
            argument.binding.data.as_deref(),
            Some(BindingData::Identifier(_))
        ) {
            return false;
        }
    }
    helpers.stmts_can_be_removed_if_unused(
        &body.block.statements,
        StmtsCanBeRemovedIfUnusedFlags::RETURN_CAN_BE_REMOVED_IF_UNUSED,
    )
}

fn maybe_inline_iife(loc: Loc, call: &CallExpr) -> Option<ExprData> {
    if !call.args.is_empty() {
        return None;
    }
    let Some(ExprData::Arrow(arrow)) = call.target.data.as_deref() else {
        return None;
    };
    if !arrow.args.is_empty() || arrow.is_async {
        return None;
    }
    let replacement = match arrow.body.block.statements.as_slice() {
        [] => Expr::new(loc, ExprData::Undefined),
        [statement] => match statement.data.as_deref() {
            Some(StmtData::Return(statement)) => {
                if statement.value_or_nil.data.is_some() {
                    statement.value_or_nil.clone()
                } else {
                    Expr::new(loc, ExprData::Undefined)
                }
            }
            Some(StmtData::Expr(statement)) => Expr::new(
                statement.value.loc,
                ExprData::Unary(UnaryExpr {
                    value: statement.value.clone(),
                    op: OpCode::UnaryVoid,
                    ..UnaryExpr::default()
                }),
            ),
            _ => return None,
        },
        _ => return None,
    };
    let replacement = if call.can_be_unwrapped_if_unused {
        Expr::new(
            loc,
            ExprData::Annotation(AnnotationExpr {
                value: replacement,
                flags: AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED,
            }),
        )
    } else {
        replacement
    };
    replacement.data.map(|data| *data)
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

fn maybe_fold_object_property_access(
    core: &ParserCore,
    target: &Expr,
    name: &str,
) -> Option<ExprData> {
    let ExprData::Object(object) = target.data.as_deref()? else {
        return None;
    };
    let mut replacement = None;
    let mut has_proto_null = false;
    for property in &object.properties {
        if property.kind == PropertyKind::Spread
            || property.flags.contains(PropertyFlags::IS_COMPUTED)
            || property.kind.is_method_definition()
        {
            return None;
        }
        let Some(ExprData::String(key)) = property.key.data.as_deref() else {
            return None;
        };
        let is_proto = crate::internal::helpers::utf16_equals_wtf8(&key.value, b"__proto__");
        if is_proto && matches!(property.value_or_nil.data.as_deref(), Some(ExprData::Null)) {
            has_proto_null = true;
        }
        if !expression_can_be_removed_if_unused(core, &property.value_or_nil) {
            return None;
        }
        if crate::internal::helpers::utf16_equals_wtf8(&key.value, name.as_bytes()) {
            replacement = property.value_or_nil.data.as_deref().cloned();
        }
    }
    if name != "__proto__"
        && let Some(replacement) = replacement
    {
        return Some(replacement);
    }
    has_proto_null.then_some(ExprData::Undefined)
}

#[derive(Clone, Copy, Debug, Default)]
struct ExprVisitContext {
    is_call_target: bool,
    is_property_access_target: bool,
    is_template_tag: bool,
}

fn value_to_substitute_for_require(core: &mut ParserCore, loc: Loc) -> Expr {
    if core.source.index != crate::internal::runtime::SOURCE_INDEX
        && crate::internal::config::should_call_runtime_require(
            core.options.mode,
            core.options.output_format,
        )
    {
        core.import_from_runtime(loc, "__require")
    } else {
        core.record_usage(core.require_ref);
        Expr::new(
            loc,
            ExprData::Identifier(IdentifierExpr {
                reference: core.require_ref,
                ..IdentifierExpr::default()
            }),
        )
    }
}

fn ignore_usage_if_recorded(core: &mut ParserCore, reference: Ref) {
    if core
        .symbol_uses
        .get(&reference)
        .is_some_and(|usage| usage.count_estimate > 0)
    {
        core.ignore_usage(reference);
    }
}

fn type_script_enum_constant_from_expr(
    expression: &Expr,
) -> Option<crate::internal::js_ast::TsEnumValue> {
    match expression.data.as_deref()? {
        ExprData::Number(number) => Some(crate::internal::js_ast::TsEnumValue {
            number: *number,
            ..crate::internal::js_ast::TsEnumValue::default()
        }),
        ExprData::String(string) => Some(crate::internal::js_ast::TsEnumValue {
            string: string.value.clone(),
            is_string: true,
            ..crate::internal::js_ast::TsEnumValue::default()
        }),
        ExprData::InlinedEnum(inlined) => type_script_enum_constant_from_expr(&inlined.value),
        _ => None,
    }
}

fn type_script_enum_value_for_property(
    core: &ParserCore,
    reference: Ref,
    name: &str,
) -> Option<crate::internal::js_ast::TsEnumValue> {
    let reference = type_script_namespace_reference(core, reference)?;
    core.ts_enums
        .get(&reference)
        .and_then(|values| values.get(name))
        .cloned()
}

fn type_script_namespace_reference(core: &ParserCore, reference: Ref) -> Option<Ref> {
    let linked = core.follow_symbol_link(reference);
    if let Some(owner) = core
        .ts_namespace_owner
        .get(&reference)
        .or_else(|| core.ts_namespace_owner.get(&linked))
        .copied()
    {
        return Some(core.follow_symbol_link(owner));
    }
    let alias = core.symbols[usize::try_from(linked.inner_index).expect("symbol index")]
        .namespace_alias
        .as_ref()?;
    let parent = type_script_namespace_reference(core, alias.namespace_ref)?;
    let member = core.ts_namespace_members.get(&parent)?.get(&alias.alias)?;
    let crate::internal::js_ast::TsNamespaceMemberData::Namespace(namespace) = &member.data else {
        return None;
    };
    type_script_namespace_reference(core, namespace.reference)
}

fn type_script_namespace_path(expression: &Expr, path: &mut Vec<String>) -> Option<Ref> {
    match expression.data.as_deref()? {
        ExprData::Identifier(identifier) => Some(identifier.reference),
        ExprData::Dot(dot) => {
            let reference = type_script_namespace_path(&dot.target, path)?;
            path.push(dot.name.clone());
            Some(reference)
        }
        ExprData::Index(index) => {
            let reference = type_script_namespace_path(&index.target, path)?;
            let ExprData::String(string) = index.index.data.as_deref()? else {
                return None;
            };
            path.push(
                String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(&string.value))
                    .into_owned(),
            );
            Some(reference)
        }
        _ => None,
    }
}

fn type_script_enum_value_for_access(
    core: &ParserCore,
    target: &Expr,
    name: &str,
) -> Option<crate::internal::js_ast::TsEnumValue> {
    let mut path = Vec::new();
    let mut reference = type_script_namespace_path(target, &mut path)?;
    reference = type_script_namespace_reference(core, reference)?;
    for segment in path {
        let member = core.ts_namespace_members.get(&reference)?.get(&segment)?;
        let crate::internal::js_ast::TsNamespaceMemberData::Namespace(namespace) = &member.data
        else {
            return None;
        };
        reference = type_script_namespace_reference(core, namespace.reference)?;
    }
    type_script_enum_value_for_property(core, reference, name)
}

fn type_script_enum_value_for_identifier(
    core: &ParserCore,
    reference: Ref,
) -> Option<(crate::internal::js_ast::TsEnumValue, String)> {
    let linked = core.follow_symbol_link(reference);
    if let Some(value) = core
        .ts_enum_values_by_ref
        .get(&reference)
        .or_else(|| core.ts_enum_values_by_ref.get(&linked))
        .cloned()
    {
        let comment = core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
            .original_name
            .clone();
        return Some((value, comment));
    }

    let alias = core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
        .namespace_alias
        .as_ref()?;
    type_script_enum_value_for_property(core, alias.namespace_ref, &alias.alias)
        .map(|value| (value, alias.alias.clone()))
}

fn visit_expr_with_target(
    core: &mut ParserCore,
    expression: &mut Expr,
    resolve_identifiers: bool,
    assign_target: AssignTarget,
) {
    stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || {
        visit_expr_with_target_and_context(
            core,
            expression,
            resolve_identifiers,
            assign_target,
            ExprVisitContext::default(),
        );
    });
}

#[allow(clippy::too_many_lines)]
fn visit_expr_with_target_and_context(
    core: &mut ParserCore,
    expression: &mut Expr,
    resolve_identifiers: bool,
    assign_target: AssignTarget,
    context: ExprVisitContext,
) {
    let expression_loc = expression.loc;
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
            if identifier.reference == core.require_ref
                && !context.is_call_target
                && !context.is_property_access_target
            {
                ignore_usage_if_recorded(core, identifier.reference);
                if let Some(replacement) =
                    value_to_substitute_for_require(core, expression.loc).data
                {
                    *data = *replacement;
                }
                return;
            }
            let current_scope_contains_direct_eval =
                core.current_scope.as_ref().is_some_and(|scope| {
                    scope
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_direct_eval
                });
            if assign_target == AssignTarget::None
                && core.options.minify_syntax
                && !current_scope_contains_direct_eval
                && !identifier.must_keep_due_to_with_stmt
                && let Some(value) = core.const_values.get(&identifier.reference)
                && let Some(replacement) =
                    crate::internal::js_ast::const_value_to_expr(expression.loc, value).data
            {
                core.ignore_usage(identifier.reference);
                *data = *replacement;
                return;
            }
            if assign_target == AssignTarget::None {
                let symbol = &core.symbols[identifier.reference.inner_index as usize];
                identifier.call_can_be_unwrapped_if_unused |= !core.options.ignore_dce_annotations
                    && symbol
                        .flags
                        .contains(SymbolFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED);
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
            } else if let Some((value, comment)) =
                type_script_enum_value_for_identifier(core, identifier.reference)
            {
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
            } else if assign_target == AssignTarget::None
                && core.is_import_item.contains(&identifier.reference)
            {
                *data = ExprData::ImportIdentifier(crate::internal::js_ast::ImportIdentifierExpr {
                    reference: identifier.reference,
                    was_originally_identifier: true,
                    ..crate::internal::js_ast::ImportIdentifierExpr::default()
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
            if assign_target != AssignTarget::None {
                core.mark_syntax_feature(
                    JsFeature::DESTRUCTURING,
                    Range {
                        loc: expression_loc,
                        len: 1,
                    },
                );
            }
            let mut has_spread = false;
            for item in &mut array.items {
                has_spread |= matches!(item.data.as_deref(), Some(ExprData::Spread(_)));
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
            if core.options.minify_syntax && has_spread && assign_target == AssignTarget::None {
                array.items = inline_spreads_of_array_literals(&array.items);
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
            match unary.op {
                OpCode::UnaryTypeof => {
                    if let Some(value) = crate::internal::js_ast::typeof_without_side_effects(
                        unary.value.data.as_deref(),
                    ) {
                        *data = ExprData::String(StringExpr {
                            value: string_to_utf16(value.as_bytes()),
                            ..StringExpr::default()
                        });
                        return;
                    }
                }
                OpCode::UnaryNot => {
                    if core.options.minify_syntax {
                        let helpers = make_helper_context(|reference| {
                            core.symbols
                                [usize::try_from(reference.inner_index).expect("symbol index")]
                            .kind
                                == SymbolKind::Unbound
                        });
                        unary.value = helpers.simplify_boolean_expr(&unary.value);
                    }
                    if let Some((value, crate::internal::js_ast::SideEffects::NoSideEffects)) =
                        crate::internal::js_ast::to_boolean_with_side_effects(
                            unary.value.data.as_deref(),
                        )
                    {
                        *data = ExprData::Boolean(!value);
                        return;
                    }
                    if core.options.minify_syntax
                        && let Some(replacement) =
                            crate::internal::js_ast::maybe_simplify_not(&unary.value)
                        && let Some(replacement) = replacement.data
                    {
                        *data = *replacement;
                        return;
                    }
                }
                OpCode::UnaryVoid => {
                    let should_remove = if core.options.minify_syntax {
                        let helpers = make_helper_context(|reference| {
                            core.symbols
                                [usize::try_from(reference.inner_index).expect("symbol index")]
                            .kind
                                == SymbolKind::Unbound
                        });
                        helpers.expr_can_be_removed_if_unused(&unary.value)
                    } else {
                        is_unsightly_primitive(unary.value.data.as_deref())
                    };
                    if should_remove {
                        *data = ExprData::Undefined;
                        return;
                    }
                }
                OpCode::UnaryPositive | OpCode::UnaryNegative => {
                    if let Some(mut number) =
                        crate::internal::js_ast::to_number_without_side_effects(
                            unary.value.data.as_deref(),
                        )
                    {
                        if unary.op == OpCode::UnaryNegative {
                            number = -number;
                        }
                        *data = ExprData::Number(number);
                        return;
                    }
                }
                OpCode::UnaryComplement
                    if core.should_fold_type_script_constant_expressions
                        || core.options.minify_syntax =>
                {
                    if let Some(number) = crate::internal::js_ast::to_number_without_side_effects(
                        unary.value.data.as_deref(),
                    ) {
                        *data =
                            ExprData::Number(f64::from(!crate::internal::js_ast::to_int32(number)));
                        return;
                    }
                }
                _ => {}
            }
            if core.options.minify_syntax
                && !matches!(unary.op, OpCode::UnaryDelete | OpCode::UnaryTypeof)
                && let Some(ExprData::Binary(comma)) = unary.value.data.as_deref()
                && comma.op == OpCode::BinaryComma
            {
                let replacement = join_with_comma(
                    comma.left.clone(),
                    Expr::new(
                        comma.right.loc,
                        ExprData::Unary(UnaryExpr {
                            value: comma.right.clone(),
                            op: unary.op,
                            ..UnaryExpr::default()
                        }),
                    ),
                );
                if let Some(replacement) = replacement.data {
                    *data = *replacement;
                    return;
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
            let right_is_dead = match binary.op {
                OpCode::BinaryLogicalOr => crate::internal::js_ast::to_boolean_with_side_effects(
                    binary.left.data.as_deref(),
                )
                .is_some_and(|(value, _)| value),
                OpCode::BinaryLogicalAnd => crate::internal::js_ast::to_boolean_with_side_effects(
                    binary.left.data.as_deref(),
                )
                .is_some_and(|(value, _)| !value),
                OpCode::BinaryNullishCoalescing => {
                    crate::internal::js_ast::to_null_or_undefined_with_side_effects(
                        binary.left.data.as_deref(),
                    )
                    .is_some_and(|(value, _)| !value)
                }
                _ => false,
            };
            let old_control_flow_dead = core.is_control_flow_dead;
            core.is_control_flow_dead |= right_is_dead;
            visit_expr(core, &mut binary.right, resolve_identifiers);
            core.is_control_flow_dead = old_control_flow_dead;
            keep_inferred_name(core, &mut binary.right, inferred_name);
            if core.options.minify_syntax
                && matches!(
                    binary.op,
                    OpCode::BinaryLooseEqual
                        | OpCode::BinaryLooseNotEqual
                        | OpCode::BinaryStrictEqual
                        | OpCode::BinaryStrictNotEqual
                )
                && is_primitive_literal(binary.left.data.as_deref())
                && !is_primitive_literal(binary.right.data.as_deref())
            {
                std::mem::swap(&mut binary.left, &mut binary.right);
            }
            if (core.should_fold_type_script_constant_expressions
                || (core.options.minify_syntax
                    && crate::internal::js_ast::should_fold_binary_operator_when_minifying(binary))
                || matches!(
                    binary.op,
                    OpCode::BinaryLogicalOr
                        | OpCode::BinaryLogicalAnd
                        | OpCode::BinaryNullishCoalescing
                ))
                && let Some(folded) =
                    crate::internal::js_ast::fold_binary_operator(expression.loc, binary)
                && let Some(folded) = folded.data
            {
                *data = *folded;
                return;
            }
            if binary.op == OpCode::BinaryPower
                && core
                    .options
                    .unsupported_js_features
                    .contains(JsFeature::EXPONENT_OPERATOR)
            {
                let lowered = core.call_runtime(
                    expression.loc,
                    "__pow",
                    vec![
                        std::mem::take(&mut binary.left),
                        std::mem::take(&mut binary.right),
                    ],
                );
                if let Some(lowered) = lowered.data {
                    *data = *lowered;
                }
                return;
            }
            if binary.op == OpCode::BinaryAdd {
                if let Some(folded) = crate::internal::js_ast::fold_string_addition(
                    binary.left.clone(),
                    binary.right.clone(),
                    crate::internal::js_ast::StringAdditionKind::Normal,
                ) && let Some(folded) = folded.data
                {
                    *data = *folded;
                    return;
                }
                if let Some(ExprData::Binary(left)) = binary.left.data.as_deref()
                    && left.op == OpCode::BinaryAdd
                    && let Some(folded) = crate::internal::js_ast::fold_string_addition(
                        left.right.clone(),
                        binary.right.clone(),
                        crate::internal::js_ast::StringAdditionKind::WithNestedLeft,
                    )
                {
                    *data = ExprData::Binary(BinaryExpr {
                        left: left.left.clone(),
                        right: folded,
                        op: left.op,
                    });
                    return;
                }
            }
            if core.options.minify_syntax && binary.op == OpCode::BinaryComma {
                let helpers = make_helper_context(|reference| {
                    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                        == SymbolKind::Unbound
                });
                binary.left = helpers
                    .simplify_unused_expr(&binary.left, core.options.unsupported_js_features);
                if binary.left.data.is_none()
                    && let Some(right) = binary.right.data.as_deref().cloned()
                {
                    expression.loc = binary.right.loc;
                    *data = right;
                    return;
                }
            }
            let equality = match binary.op {
                OpCode::BinaryLooseEqual => {
                    Some((crate::internal::js_ast::EqualityKind::Loose, false))
                }
                OpCode::BinaryLooseNotEqual => {
                    Some((crate::internal::js_ast::EqualityKind::Loose, true))
                }
                OpCode::BinaryStrictEqual => {
                    Some((crate::internal::js_ast::EqualityKind::Strict, false))
                }
                OpCode::BinaryStrictNotEqual => {
                    Some((crate::internal::js_ast::EqualityKind::Strict, true))
                }
                _ => None,
            };
            if let Some((kind, negate)) = equality {
                if let Some(equal) = crate::internal::js_ast::check_equality_if_no_side_effects(
                    binary.left.data.as_deref(),
                    binary.right.data.as_deref(),
                    kind,
                ) {
                    *data = ExprData::Boolean(if negate { !equal } else { equal });
                    return;
                }
                if core.options.minify_syntax {
                    match binary.op {
                        OpCode::BinaryLooseEqual | OpCode::BinaryLooseNotEqual => {
                            if matches!(binary.left.data.as_deref(), Some(ExprData::Undefined)) {
                                binary.left = Expr::new(binary.left.loc, ExprData::Null);
                            } else if matches!(
                                binary.right.data.as_deref(),
                                Some(ExprData::Undefined)
                            ) {
                                binary.right = Expr::new(binary.right.loc, ExprData::Null);
                            }
                        }
                        OpCode::BinaryStrictEqual | OpCode::BinaryStrictNotEqual
                            if crate::internal::js_ast::can_change_strict_to_loose(
                                &binary.left,
                                &binary.right,
                            ) =>
                        {
                            binary.op = if binary.op == OpCode::BinaryStrictEqual {
                                OpCode::BinaryLooseEqual
                            } else {
                                OpCode::BinaryLooseNotEqual
                            };
                        }
                        _ => {}
                    }
                    if let Some(replacement) =
                        crate::internal::js_ast::maybe_simplify_equality_comparison(
                            expression.loc,
                            binary,
                            core.options.unsupported_js_features,
                        )
                        && let Some(replacement) = replacement.data
                    {
                        *data = *replacement;
                        return;
                    }
                }
            }
            if core.options.minify_syntax
                && binary.op != OpCode::BinaryComma
                && let Some(ExprData::Binary(comma)) = binary.left.data.as_deref()
                && comma.op == OpCode::BinaryComma
            {
                let replacement = join_with_comma(
                    comma.left.clone(),
                    Expr::new(
                        comma.right.loc,
                        ExprData::Binary(BinaryExpr {
                            left: comma.right.clone(),
                            right: binary.right.clone(),
                            op: binary.op,
                        }),
                    ),
                );
                if let Some(replacement) = replacement.data {
                    *data = *replacement;
                    return;
                }
            }
        }
        ExprData::New(new) => {
            visit_expr_with_target_and_context(
                core,
                &mut new.target,
                resolve_identifiers,
                AssignTarget::None,
                ExprVisitContext {
                    is_call_target: true,
                    ..ExprVisitContext::default()
                },
            );
            let mut has_spread = false;
            for argument in &mut new.args {
                has_spread |= matches!(argument.data.as_deref(), Some(ExprData::Spread(_)));
                visit_expr(core, argument, resolve_identifiers);
            }
            if core.options.minify_syntax && has_spread {
                new.args = inline_spreads_of_array_literals(&new.args);
            }
        }
        ExprData::Call(call) => {
            let target_was_identifier =
                matches!(call.target.data.as_deref(), Some(ExprData::Identifier(_)));
            visit_expr_with_target_and_context(
                core,
                &mut call.target,
                resolve_identifiers,
                AssignTarget::None,
                ExprVisitContext {
                    is_call_target: true,
                    ..ExprVisitContext::default()
                },
            );
            if core.options.minify_syntax {
                let collapse_indirect_identifier = match call.target.data.as_deref() {
                    Some(ExprData::Binary(binary))
                        if binary.op == OpCode::BinaryComma
                            && matches!(
                                binary.left.data.as_deref(),
                                Some(ExprData::Number(number)) if *number == 0.0
                            ) =>
                    {
                        match binary.right.data.as_deref() {
                            Some(ExprData::Identifier(identifier)) => {
                                let symbol = &core.symbols[usize::try_from(
                                    identifier.reference.inner_index,
                                )
                                .expect("symbol index")];
                                if symbol.kind != SymbolKind::Unbound
                                    || symbol.original_name != "eval"
                                {
                                    Some(binary.right.clone())
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(target) = collapse_indirect_identifier {
                    call.target = target;
                }
            }
            if let Some(ExprData::Function(function)) = call.target.data.as_deref_mut() {
                function.is_parenthesized = true;
            }
            call.can_be_unwrapped_if_unused |= match call.target.data.as_deref() {
                Some(ExprData::Identifier(identifier)) => {
                    identifier.call_can_be_unwrapped_if_unused
                }
                Some(ExprData::Dot(dot)) => dot.call_can_be_unwrapped_if_unused,
                Some(ExprData::Index(index)) => index.call_can_be_unwrapped_if_unused,
                _ => false,
            };
            let mut has_spread = false;
            for argument in &mut call.args {
                has_spread |= matches!(argument.data.as_deref(), Some(ExprData::Spread(_)));
                visit_expr(core, argument, resolve_identifiers);
            }
            if core.options.minify_syntax && has_spread {
                call.args = inline_spreads_of_array_literals(&call.args);
            }
            if core.options.minify_syntax && !core.is_control_flow_dead {
                let reference = match call.target.data.as_deref() {
                    Some(ExprData::Identifier(identifier)) => Some(identifier.reference),
                    Some(ExprData::ImportIdentifier(identifier)) => Some(identifier.reference),
                    _ => None,
                };
                if let Some(reference) = reference {
                    core.convert_symbol_use_to_call(reference, call.args.len() == 1 && !has_spread);
                }
            }
            if !call.can_be_unwrapped_if_unused {
                call.can_be_unwrapped_if_unused = match call.target.data.as_deref() {
                    Some(ExprData::Arrow(arrow)) => {
                        !arrow.is_async
                            && iife_can_be_removed_if_unused(core, &arrow.args, &arrow.body)
                    }
                    Some(ExprData::Function(function)) => {
                        !function.function.is_async
                            && !function.function.is_generator
                            && iife_can_be_removed_if_unused(
                                core,
                                &function.function.args,
                                &function.function.body,
                            )
                    }
                    _ => false,
                };
            }
            if !call.can_be_unwrapped_if_unused {
                call.can_be_unwrapped_if_unused = if call.args.len() <= 1
                    && is_unbound_identifier_named(core, &call.target, "Symbol")
                {
                    call.args.first().is_none_or(|argument| {
                        known_primitive_type(argument.data.as_deref()) != PrimitiveType::Unknown
                    })
                } else if call.args.len() == 1
                    && let Some(ExprData::Dot(dot)) = call.target.data.as_deref()
                    && dot.name == "for"
                    && is_unbound_identifier_named(core, &dot.target, "Symbol")
                {
                    known_primitive_type(call.args[0].data.as_deref()) != PrimitiveType::Unknown
                } else {
                    false
                };
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
            if core.options.minify_syntax
                && let Some(replacement) = maybe_inline_iife(expression.loc, call)
            {
                *data = replacement;
                return;
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
            if let Some(kind) = kind {
                if kind == crate::internal::ast::ImportKind::Require {
                    ignore_usage_if_recorded(core, core.require_ref);
                }
                if call.args.len() == 1
                    && let Some(ExprData::String(path)) = call.args[0].data.as_deref()
                {
                    if kind == crate::internal::ast::ImportKind::Require
                        && core.is_control_flow_dead
                    {
                        *data = ExprData::Null;
                        return;
                    }
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
                    if kind == crate::internal::ast::ImportKind::Require
                        && core.visit_try_body_depth > 0
                    {
                        let record = &mut core.import_records[import_record_index as usize];
                        record.flags |= ImportRecordFlags::HANDLES_IMPORT_ERRORS;
                        record.error_handler_loc = core.visit_try_catch_loc;
                    }
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
                } else if kind == crate::internal::ast::ImportKind::Require {
                    if call.args.len() == 1
                        && let Some(replacement) = handle_glob_pattern(
                            core,
                            call.args[0].clone(),
                            ImportKind::Require,
                            crate::internal::ast::ImportPhase::Evaluation,
                            None,
                            ImportRecordFlags::default(),
                        )
                        && let Some(replacement) = replacement.data
                    {
                        *data = *replacement;
                    } else {
                        call.target = value_to_substitute_for_require(core, call.target.loc);
                    }
                }
            }
        }
        ExprData::Dot(dot) => {
            visit_expr_with_target_and_context(
                core,
                &mut dot.target,
                resolve_identifiers,
                AssignTarget::None,
                ExprVisitContext {
                    is_property_access_target: true,
                    ..ExprVisitContext::default()
                },
            );
            if assign_target == AssignTarget::None
                && dot.optional_chain == OptionalChain::None
                && let Some(ExprData::ImportIdentifier(identifier)) = dot.target.data.as_deref()
                && core.is_import_item.contains(&identifier.reference)
            {
                core.record_import_symbol_property_use(identifier.reference, dot.name.clone());
            }
            if assign_target == AssignTarget::None
                && let Some(replacement) = maybe_rewrite_import_namespace_property(
                    core,
                    &dot.target,
                    &dot.name,
                    dot.name_loc,
                    false,
                )
            {
                *data = replacement;
                return;
            }
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
            let replacement = if assign_target == AssignTarget::None
                && dot.optional_chain == OptionalChain::None
            {
                type_script_enum_value_for_access(core, &dot.target, &dot.name).map(|value| {
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
                if let Some(ExprData::Identifier(identifier)) = dot.target.data.as_deref() {
                    core.ignore_usage(identifier.reference);
                }
                *data = replacement;
                return;
            }
            if core.options.minify_syntax
                && assign_target == AssignTarget::None
                && dot.optional_chain == OptionalChain::None
                && !context.is_call_target
                && !context.is_template_tag
                && let Some(replacement) =
                    maybe_fold_object_property_access(core, &dot.target, &dot.name)
            {
                *data = replacement;
                return;
            }
        }
        ExprData::Index(index) => {
            visit_expr_with_target_and_context(
                core,
                &mut index.target,
                resolve_identifiers,
                AssignTarget::None,
                ExprVisitContext {
                    is_property_access_target: true,
                    ..ExprVisitContext::default()
                },
            );
            visit_expr(core, &mut index.index, resolve_identifiers);
            if assign_target == AssignTarget::None
                && index.optional_chain == OptionalChain::None
                && let Some(ExprData::ImportIdentifier(identifier)) = index.target.data.as_deref()
                && core.is_import_item.contains(&identifier.reference)
                && let Some(ExprData::String(string)) = index.index.data.as_deref()
            {
                let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                    &string.value,
                ))
                .into_owned();
                core.record_import_symbol_property_use(identifier.reference, name);
            }
            if assign_target == AssignTarget::None
                && let Some(ExprData::String(string)) = index.index.data.as_deref()
            {
                let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                    &string.value,
                ))
                .into_owned();
                if let Some(replacement) = maybe_rewrite_import_namespace_property(
                    core,
                    &index.target,
                    &name,
                    index.index.loc,
                    true,
                ) {
                    *data = replacement;
                    return;
                }
            }
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
            let replacement = if assign_target == AssignTarget::None
                && index.optional_chain == OptionalChain::None
            {
                let name = match index.index.data.as_deref() {
                    Some(ExprData::String(string)) => Some(
                        String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                            &string.value,
                        ))
                        .into_owned(),
                    ),
                    _ => None,
                };
                name.and_then(|name| {
                    type_script_enum_value_for_access(core, &index.target, &name)
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
                if let Some(ExprData::Identifier(identifier)) = index.target.data.as_deref() {
                    core.ignore_usage(identifier.reference);
                }
                *data = replacement;
                return;
            }
            if core.options.minify_syntax
                && assign_target == AssignTarget::None
                && index.optional_chain == OptionalChain::None
                && !context.is_call_target
                && !context.is_template_tag
                && let Some(ExprData::String(string)) = index.index.data.as_deref()
            {
                let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                    &string.value,
                ))
                .into_owned();
                if let Some(replacement) =
                    maybe_fold_object_property_access(core, &index.target, &name)
                {
                    *data = replacement;
                    return;
                }
            }
            if core.options.minify_syntax
                && let Some(ExprData::String(string)) = index.index.data.as_deref()
            {
                let name = String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(
                    &string.value,
                ))
                .into_owned();
                if is_identifier(&name) {
                    *data = ExprData::Dot(DotExpr {
                        target: std::mem::take(&mut index.target),
                        name,
                        name_loc: index.index.loc,
                        optional_chain: index.optional_chain,
                        can_be_removed_if_unused: index.can_be_removed_if_unused,
                        call_can_be_unwrapped_if_unused: index.call_can_be_unwrapped_if_unused,
                        is_symbol_instance: index.is_symbol_instance,
                    });
                }
            }
        }
        ExprData::Object(object) => {
            if assign_target != AssignTarget::None {
                core.mark_syntax_feature(
                    JsFeature::DESTRUCTURING,
                    Range {
                        loc: expression_loc,
                        len: 1,
                    },
                );
            }
            report_duplicate_properties(core, &object.properties, DuplicatePropertiesIn::Object);
            if assign_target == AssignTarget::None {
                report_duplicate_proto_properties(core, &object.properties);
            }
            let mut has_spread = false;
            for property in &mut object.properties {
                has_spread |= property.kind == PropertyKind::Spread;
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
            if core.options.minify_syntax && has_spread && assign_target == AssignTarget::None {
                object.properties = mangle_object_spread(&object.properties);
            }
            if assign_target == AssignTarget::None
                && let Some(replacement) = lower_object_spread(core, expression_loc, object)
            {
                *data = replacement;
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
            let is_template_tag = template.tag_or_nil.data.is_some();
            visit_expr_with_target_and_context(
                core,
                &mut template.tag_or_nil,
                resolve_identifiers,
                AssignTarget::None,
                ExprVisitContext {
                    is_template_tag,
                    ..ExprVisitContext::default()
                },
            );
            if let Some(ExprData::Identifier(identifier)) = template.tag_or_nil.data.as_deref()
                && identifier.call_can_be_unwrapped_if_unused
            {
                template.can_be_unwrapped_if_unused = true;
            }
            for part in &mut template.parts {
                visit_expr(core, &mut part.value, resolve_identifiers);
            }
            if core.should_fold_type_script_constant_expressions || core.options.minify_syntax {
                let replacement = inline_primitives_into_template(expression.loc, template);
                if let Some(replacement) = replacement.data {
                    *data = *replacement;
                    return;
                }
            }
            if template.tag_or_nil.data.is_some()
                && !core
                    .options
                    .unsupported_js_features
                    .contains(JsFeature::INLINE_SCRIPT)
                && (contains_closing_script_tag(&template.head_raw)
                    || template
                        .parts
                        .iter()
                        .any(|part| contains_closing_script_tag(&part.tail_raw)))
            {
                let replacement = lower_tagged_template(core, expression.loc, template);
                if let Some(replacement) = replacement.data {
                    *data = *replacement;
                    return;
                }
            }
        }
        ExprData::InlinedEnum(inlined) => {
            visit_expr(core, &mut inlined.value, resolve_identifiers);
        }
        ExprData::Annotation(annotation) => {
            visit_expr(core, &mut annotation.value, resolve_identifiers);
        }
        ExprData::Await(await_expression) => {
            let strip_dead_top_level_await = core.visit_is_outside_fn_or_arrow
                && core.is_control_flow_dead
                && (core
                    .options
                    .unsupported_js_features
                    .contains(JsFeature::TOP_LEVEL_AWAIT)
                    || !core.options.output_format.keep_esm_import_export_syntax());
            if core.visit_is_outside_fn_or_arrow && !strip_dead_top_level_await {
                let range = Range {
                    loc: expression.loc,
                    len: 5,
                };
                core.live_top_level_await_keyword = range;
                core.mark_syntax_feature(JsFeature::TOP_LEVEL_AWAIT, range);
            }
            visit_expr(core, &mut await_expression.value, resolve_identifiers);
            if strip_dead_top_level_await {
                if let Some(value) = await_expression.value.data.take() {
                    *data = *value;
                }
                return;
            }
            if core.visit_try_body_depth > 0
                && let Some(ExprData::ImportString(import)) = await_expression.value.data.as_deref()
            {
                let record = &mut core.import_records[import.import_record_index as usize];
                record.flags |= ImportRecordFlags::HANDLES_IMPORT_ERRORS;
                record.error_handler_loc = core.visit_try_catch_loc;
            }
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
            if core.options.minify_syntax {
                let helpers = make_helper_context(|reference| {
                    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                        == SymbolKind::Unbound
                });
                if_expression.test = helpers.simplify_boolean_expr(&if_expression.test);
            }
            if let Some((boolean, side_effects)) =
                crate::internal::js_ast::to_boolean_with_side_effects(
                    if_expression.test.data.as_deref(),
                )
            {
                let old_control_flow_dead = core.is_control_flow_dead;
                let live = if boolean {
                    visit_expr(core, &mut if_expression.yes, resolve_identifiers);
                    core.is_control_flow_dead = true;
                    visit_expr(core, &mut if_expression.no, resolve_identifiers);
                    if_expression.yes.clone()
                } else {
                    core.is_control_flow_dead = true;
                    visit_expr(core, &mut if_expression.yes, resolve_identifiers);
                    core.is_control_flow_dead = old_control_flow_dead;
                    visit_expr(core, &mut if_expression.no, resolve_identifiers);
                    if_expression.no.clone()
                };
                core.is_control_flow_dead = old_control_flow_dead;
                if core.options.minify_syntax {
                    let replacement = if side_effects
                        == crate::internal::js_ast::SideEffects::CouldHaveSideEffects
                    {
                        let helpers = make_helper_context(|reference| {
                            core.symbols
                                [usize::try_from(reference.inner_index).expect("symbol index")]
                            .kind
                                == SymbolKind::Unbound
                        });
                        join_with_comma(
                            helpers.simplify_unused_expr(
                                &if_expression.test,
                                core.options.unsupported_js_features,
                            ),
                            live,
                        )
                    } else {
                        live
                    };
                    if let Some(replacement) = replacement.data {
                        *data = *replacement;
                    }
                    return;
                }
            } else {
                visit_expr(core, &mut if_expression.yes, resolve_identifiers);
                visit_expr(core, &mut if_expression.no, resolve_identifiers);
            }
            if core.options.minify_syntax {
                let helpers = make_helper_context(|reference| {
                    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind
                        == SymbolKind::Unbound
                });
                let replacement = helpers.mangle_if_expr(
                    expression.loc,
                    if_expression,
                    core.options.unsupported_js_features,
                );
                if let Some(replacement) = replacement.data {
                    *data = *replacement;
                }
                return;
            }
        }
        ExprData::ImportCall(import) => {
            visit_expr(core, &mut import.expr, resolve_identifiers);
            visit_expr(core, &mut import.options_or_nil, resolve_identifiers);
            let options = import_options(&import.options_or_nil);
            if (import.options_or_nil.data.is_none() || options.is_some())
                && let Some(ExprData::String(path)) = import.expr.data.as_deref()
            {
                let (assert_or_with, flags) = options.clone().unwrap_or_default();
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
            } else if import.options_or_nil.data.is_none() || options.is_some() {
                let (assert_or_with, flags) = options.unwrap_or_default();
                if let Some(replacement) = handle_glob_pattern(
                    core,
                    import.expr.clone(),
                    ImportKind::Dynamic,
                    import.phase,
                    assert_or_with,
                    flags,
                ) && let Some(replacement) = replacement.data
                {
                    *data = *replacement;
                }
            }
        }
        ExprData::Function(function) => {
            let name = function.function.name;
            let name_to_keep = if core.options.keep_names {
                name.map(|name| symbol_name(core, name.reference))
            } else {
                None
            };
            visit_function(core, &mut function.function, resolve_identifiers);
            if core.options.minify_syntax
                && name.is_some_and(|name| {
                    core.symbols[usize::try_from(name.reference.inner_index).expect("symbol index")]
                        .use_count_estimate
                        == 0
                })
            {
                function.function.name = None;
            }
            keep_name = name_to_keep;
        }
        ExprData::Class(class) => {
            let name_to_keep = if core.options.keep_names {
                class
                    .class
                    .name
                    .map(|name| symbol_name(core, name.reference))
            } else {
                None
            };
            visit_class(core, &mut class.class, resolve_identifiers, true);
            if let Some(name) = name_to_keep {
                insert_class_name_static_block(core, &mut class.class, &name);
            }
        }
        ExprData::Arrow(arrow) => {
            let old_loop_depth = std::mem::take(&mut core.visit_loop_depth);
            let old_switch_depth = std::mem::take(&mut core.visit_switch_depth);
            let old_try_body_depth = std::mem::take(&mut core.visit_try_body_depth);
            let old_try_catch_loc = core.visit_try_catch_loc;
            let old_is_async_generator =
                std::mem::replace(&mut core.visit_is_async_generator, false);
            let old_is_outside_fn_or_arrow =
                std::mem::replace(&mut core.visit_is_outside_fn_or_arrow, false);
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
            }
            core.push_scope_for_visit_pass(ScopeKind::FunctionBody, arrow.body.loc);
            visit_statements(core, &mut arrow.body.block.statements, resolve_identifiers);
            if core.options.keep_names && core.source.key_path.text != "<runtime>" {
                apply_keep_names_to_type_script_namespaces(core, &mut arrow.body.block.statements);
            }
            lower_nested_type_script_statements(core, &mut arrow.body.block.statements, None);
            if core.options.keep_names && core.source.key_path.text != "<runtime>" {
                apply_keep_names_to_statements(core, &mut arrow.body.block.statements);
            }
            if core.options.minify_syntax {
                optimize_implicit_jumps(
                    core,
                    &mut arrow.body.block.statements,
                    ImplicitJumpKind::Return,
                );
                merge_adjacent_returns(core, &mut arrow.body.block.statements);
                cleanup_function_body_tail(core, &mut arrow.body.block.statements);
                arrow
                    .body
                    .block
                    .statements
                    .retain(|statement| statement.data.is_some());
            }
            core.pop_scope();
            core.pop_scope();
            core.visit_loop_depth = old_loop_depth;
            core.visit_switch_depth = old_switch_depth;
            core.visit_try_body_depth = old_try_body_depth;
            core.visit_try_catch_loc = old_try_catch_loc;
            core.visit_is_async_generator = old_is_async_generator;
            core.visit_is_outside_fn_or_arrow = old_is_outside_fn_or_arrow;
            if core.options.minify_syntax
                && collapse_expression_statements_into_return(&mut arrow.body.block.statements)
            {
                arrow.prefer_expr = true;
            }
            if core
                .options
                .unsupported_js_features
                .contains(JsFeature::ARROW)
            {
                *data = ExprData::Function(crate::internal::js_ast::FunctionExpr {
                    function: crate::internal::js_ast::Function {
                        args: std::mem::take(&mut arrow.args),
                        body: std::mem::take(&mut arrow.body),
                        arguments_ref: crate::internal::ast::INVALID_REF,
                        open_paren_loc: expression.loc,
                        is_async: arrow.is_async,
                        has_rest_arg: arrow.has_rest_arg,
                        has_body: true,
                        has_no_side_effects_comment: arrow.has_no_side_effects_comment,
                        is_unique_formal_parameters: true,
                        ..crate::internal::js_ast::Function::default()
                    },
                    is_parenthesized: false,
                });
            }
        }
        ExprData::JsxElement(element) => {
            visit_expr(core, &mut element.tag_or_nil, resolve_identifiers);
            let mut has_spread = false;
            for property in &mut element.properties {
                if property.kind == PropertyKind::Spread {
                    has_spread = true;
                } else if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                    visit_expr(core, &mut property.key, resolve_identifiers);
                }
                visit_expr(core, &mut property.value_or_nil, resolve_identifiers);
            }
            if core.options.minify_syntax && has_spread {
                element.properties = mangle_object_spread(&element.properties);
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
                        let mut properties = Expr::new(
                            element.tag_or_nil.loc,
                            ExprData::Object(ObjectExpr {
                                properties: std::mem::take(&mut element.properties),
                                is_single_line: element.is_tag_single_line,
                                ..ObjectExpr::default()
                            }),
                        );
                        lower_object_spread_expression(core, &mut properties);
                        args.push(properties);
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
                    let mut properties = Expr::new(
                        element.tag_or_nil.loc,
                        ExprData::Object(ObjectExpr {
                            properties,
                            is_single_line: element.is_tag_single_line,
                            ..ObjectExpr::default()
                        }),
                    );
                    lower_object_spread_expression(core, &mut properties);
                    let mut args = vec![element.tag_or_nil.clone(), properties];
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
        | ExprData::ImportMeta(_)
        | ExprData::NameOfSymbol(_)
        | ExprData::JsxText(_)
        | ExprData::Missing
        | ExprData::RegExp(_)
        | ExprData::RequireString(_)
        | ExprData::RequireResolveString(_)
        | ExprData::ImportString(_) => {}
        ExprData::This => {
            if !core.visit_this_is_nested
                && core.options.mode != crate::internal::config::Mode::PassThrough
            {
                if core.is_file_considered_esm {
                    *data = ExprData::Undefined;
                } else {
                    core.record_usage(core.exports_ref);
                    *data = ExprData::Identifier(IdentifierExpr {
                        reference: core.exports_ref,
                        ..IdentifierExpr::default()
                    });
                }
            }
        }
        ExprData::BigInt(_) => {
            if core
                .options
                .unsupported_js_features
                .contains(JsFeature::BIGINT)
            {
                let kind = if core.visit_try_body_depth > 0
                    || is_inside_node_modules(&core.source.key_path.text)
                {
                    MsgKind::Debug
                } else {
                    MsgKind::Warning
                };
                let environment = pretty_print_target_environment(
                    &core.options.original_target_env,
                    core.options.unsupported_js_feature_overrides_mask,
                );
                let range = core.source.range_of_number(expression.loc);
                if let Some(log) = core.log.clone() {
                    log.add_id(
                        MsgId::JsBigInt,
                        kind,
                        Some(&mut core.tracker),
                        range,
                        format!(
                            "Big integer literals are not available in {environment} and may crash \
                             at run-time"
                        ),
                    );
                }
                let reference = core.make_big_int_ref();
                core.record_usage(reference);
            }
        }
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

#[derive(Default)]
struct RawGlobPart {
    text: String,
    is_wildcard: bool,
}

fn raw_glob_pattern_from_expr(expression: &Expr) -> Option<Vec<RawGlobPart>> {
    match expression.data.as_deref()? {
        ExprData::String(string) => Some(vec![RawGlobPart {
            text: String::from_utf8_lossy(&utf16_to_string(&string.value)).into_owned(),
            ..RawGlobPart::default()
        }]),
        ExprData::Template(template) if template.tag_or_nil.data.is_none() => {
            let mut result = vec![RawGlobPart {
                text: String::from_utf8_lossy(&utf16_to_string(&template.head_cooked)).into_owned(),
                ..RawGlobPart::default()
            }];
            for part in &template.parts {
                if let Some(parts) = raw_glob_pattern_from_expr(&part.value) {
                    result.extend(parts);
                } else {
                    result.push(RawGlobPart {
                        is_wildcard: true,
                        ..RawGlobPart::default()
                    });
                }
                result.push(RawGlobPart {
                    text: String::from_utf8_lossy(&utf16_to_string(&part.tail_cooked)).into_owned(),
                    ..RawGlobPart::default()
                });
            }
            Some(result)
        }
        ExprData::Binary(binary) if binary.op == OpCode::BinaryAdd => {
            let mut result = raw_glob_pattern_from_expr(&binary.left)?;
            if let Some(parts) = raw_glob_pattern_from_expr(&binary.right) {
                result.extend(parts);
            } else {
                result.push(RawGlobPart {
                    is_wildcard: true,
                    ..RawGlobPart::default()
                });
            }
            Some(result)
        }
        _ => None,
    }
}

fn handle_glob_pattern(
    core: &mut ParserCore,
    expression: Expr,
    kind: ImportKind,
    phase: crate::internal::ast::ImportPhase,
    assert_or_with: Option<crate::internal::ast::ImportAssertOrWith>,
    flags: ImportRecordFlags,
) -> Option<Expr> {
    if core.options.mode != crate::internal::config::Mode::Bundle {
        return None;
    }
    let raw = raw_glob_pattern_from_expr(&expression)?;
    let mut parts = Vec::new();
    let mut last = GlobPart::default();
    for part in raw {
        if part.is_wildcard {
            if last.wildcard == GlobWildcard::None {
                if last.prefix.ends_with('/') {
                    last.wildcard = GlobWildcard::AllIncludingSlash;
                    parts.push(last);
                    last = GlobPart {
                        prefix: "/".into(),
                        wildcard: GlobWildcard::AllExceptSlash,
                    };
                } else {
                    last.wildcard = GlobWildcard::AllExceptSlash;
                }
            }
        } else if !part.text.is_empty() {
            if last.wildcard != GlobWildcard::None {
                parts.push(last);
                last = GlobPart::default();
            }
            last.prefix.push_str(&part.text);
        }
    }
    parts.push(last);
    if parts.len() == 1 && parts[0].wildcard == GlobWildcard::None {
        return None;
    }
    if !parts[0].prefix.starts_with("./") && !parts[0].prefix.starts_with("../") {
        return None;
    }

    let pattern = crate::internal::helpers::glob_pattern_to_string(&parts);
    let key = (
        kind,
        format!("{}:{assert_or_with:?}:{pattern}", phase as u8),
    );
    let reference = if let Some(reference) = core.glob_imports.get(&key).copied() {
        reference
    } else {
        let prefix = if kind == ImportKind::Require {
            "globRequire"
        } else {
            "globImport"
        };
        let mut name = prefix.to_string();
        let mut gap = true;
        for character in pattern.chars() {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '$') {
                if gap {
                    name.push('_');
                    gap = false;
                }
                name.push(character);
            } else {
                gap = true;
            }
        }
        let import_record_index = core.add_import_record(
            ImportKind::Stmt,
            phase,
            Range {
                loc: expression.loc,
                len: 0,
            },
            pattern.clone(),
            flags,
        );
        let record = &mut core.import_records[import_record_index as usize];
        record.assert_or_with = assert_or_with;
        record.glob_pattern = Some(GlobPattern {
            parts,
            export_alias: name.clone(),
            kind,
        });
        let namespace_ref = core.new_symbol(SymbolKind::Other, format!("import_{name}"));
        let reference = core.new_symbol(SymbolKind::Import, name.clone());
        core.generated_named_imports.insert(
            reference,
            crate::internal::js_ast::NamedImport {
                alias: name.clone(),
                alias_loc: expression.loc,
                namespace_ref,
                import_record_index,
                ..crate::internal::js_ast::NamedImport::default()
            },
        );
        core.glob_import_records.insert(
            format!("{}:{pattern}", kind as u8),
            (import_record_index, namespace_ref),
        );
        core.glob_imports.insert(key, reference);
        reference
    };
    core.record_usage(reference);
    Some(Expr::new(
        expression.loc,
        ExprData::Call(CallExpr {
            target: Expr::new(
                expression.loc,
                ExprData::ImportIdentifier(crate::internal::js_ast::ImportIdentifierExpr {
                    reference,
                    was_originally_identifier: true,
                    ..crate::internal::js_ast::ImportIdentifierExpr::default()
                }),
            ),
            args: vec![expression],
            ..CallExpr::default()
        }),
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
            can_be_removed_if_unused: true,
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
                can_be_removed_if_unused: true,
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
        if let Some(log) = &core.log {
            let earlier_range = Range {
                loc: duplicate.original_loc,
                len: 0,
            };
            let note = core
                .tracker
                .msg_data(earlier_range, format!("The original key {key:?} is here:"));
            log.add_id_with_notes(
                MsgId::JsDuplicateObjectKey,
                if is_inside_node_modules(&core.source.key_path.text) {
                    MsgKind::Debug
                } else {
                    MsgKind::Warning
                },
                Some(&mut core.tracker),
                Range {
                    loc: duplicate.duplicate_loc,
                    len: 0,
                },
                format!("Duplicate key {key:?} in {context}"),
                vec![note],
            );
        }
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
