use std::collections::HashSet;

use crate::internal::{
    ast::{INVALID_REF, NamespaceAlias, Ref, SymbolKind},
    compat::JsFeature,
    js_ast::{
        Arg, ArrowExpr, BinaryExpr, Binding, BindingData, BlockStmt, ClassExpr, ClassStmt, Decl,
        DeclaredSymbol, DotExpr, EnumStmt, Expr, ExprData, ExprStmt, FunctionBody,
        IdentifierBinding, IdentifierExpr, IndexExpr, LocalKind, LocalStmt, NamespaceStmt,
        ObjectExpr, OpCode, PrimitiveType, PropertyKind, ReturnStmt, Stmt, StmtData, StringExpr,
        convert_binding_to_expr, for_each_identifier_binding, is_identifier, join_with_comma,
        known_primitive_type, make_helper_context,
    },
    logger::Loc,
};

use super::parser_core::ParserCore;

#[derive(Default)]
pub(crate) struct LowerTypeScriptContext {
    emitted: HashSet<Ref>,
}

pub(crate) fn lower_type_script_statements(
    core: &mut ParserCore,
    statements: Vec<Stmt>,
) -> Vec<Stmt> {
    let mut context = LowerTypeScriptContext::default();
    context.lower_statements(core, statements)
}

pub(crate) fn lower_nested_type_script_statements(
    core: &mut ParserCore,
    statements: &mut Vec<Stmt>,
    enclosing_namespace: Option<Ref>,
) {
    let mut context = LowerTypeScriptContext::default();
    *statements =
        context.lower_nested_statements(core, std::mem::take(statements), enclosing_namespace);
}

impl LowerTypeScriptContext {
    pub(crate) fn lower_statements(
        &mut self,
        core: &mut ParserCore,
        statements: Vec<Stmt>,
    ) -> Vec<Stmt> {
        let mut result = Vec::with_capacity(statements.len());
        for statement in statements {
            let loc = statement.loc;
            let Some(data) = statement.data else {
                if !core.options.minify_syntax {
                    result.push(statement);
                }
                continue;
            };
            match *data {
                StmtData::Enum(enumeration) => {
                    lower_enum(
                        core,
                        loc,
                        enumeration,
                        &mut self.emitted,
                        &mut result,
                        true,
                        None,
                    );
                }
                StmtData::Namespace(namespace) => {
                    lower_namespace(
                        core,
                        loc,
                        namespace,
                        &mut self.emitted,
                        &mut result,
                        true,
                        None,
                    );
                }
                other => result.push(Stmt::new(loc, other)),
            }
        }
        result
    }

    fn lower_nested_statements(
        &mut self,
        core: &mut ParserCore,
        statements: Vec<Stmt>,
        enclosing_namespace: Option<Ref>,
    ) -> Vec<Stmt> {
        let mut result = Vec::with_capacity(statements.len());
        for statement in statements {
            let loc = statement.loc;
            let Some(data) = statement.data else {
                if !core.options.minify_syntax {
                    result.push(statement);
                }
                continue;
            };
            match *data {
                StmtData::Enum(enumeration) => lower_enum(
                    core,
                    loc,
                    enumeration,
                    &mut self.emitted,
                    &mut result,
                    false,
                    enclosing_namespace,
                ),
                StmtData::Namespace(namespace) => lower_namespace(
                    core,
                    loc,
                    namespace,
                    &mut self.emitted,
                    &mut result,
                    false,
                    enclosing_namespace,
                ),
                other => result.push(Stmt::new(loc, other)),
            }
        }
        result
    }
}

fn lower_namespace(
    core: &mut ParserCore,
    loc: Loc,
    namespace: NamespaceStmt,
    emitted: &mut HashSet<Ref>,
    result: &mut Vec<Stmt>,
    is_module_scope: bool,
    enclosing_namespace: Option<Ref>,
) {
    if !namespace.has_export_declare && !namespace_has_runtime_value(&namespace.statements) {
        return;
    }
    let name_ref = follow_symbols(core, namespace.name.reference);
    if should_emit_namespace_var(core, name_ref, emitted) {
        result.push(Stmt::new(
            loc,
            StmtData::Local(LocalStmt {
                declarations: vec![Decl {
                    binding: identifier_binding(namespace.name.loc, name_ref),
                    ..Decl::default()
                }],
                kind: if is_module_scope {
                    LocalKind::Var
                } else {
                    LocalKind::Let
                },
                is_export: namespace.is_export && is_module_scope,
                ..LocalStmt::default()
            }),
        ));
    }
    let mut body = lower_namespace_body(core, namespace.argument, namespace.statements, emitted);
    if core.options.minify_syntax {
        body = join_adjacent_expression_statements(body);
    }
    let initial_value = namespace_initial_value(
        core,
        namespace.name.loc,
        name_ref,
        namespace.is_export,
        enclosing_namespace,
    );
    result.push(Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: namespace_iife(
                loc,
                namespace.argument,
                body,
                initial_value,
                core.options.minify_syntax,
            ),
            ..ExprStmt::default()
        }),
    ));
}

fn join_adjacent_expression_statements(statements: Vec<Stmt>) -> Vec<Stmt> {
    let mut result: Vec<Stmt> = Vec::with_capacity(statements.len());
    for statement in statements {
        let Some(StmtData::Expr(expression)) = statement.data.as_deref() else {
            result.push(statement);
            continue;
        };
        if let Some(StmtData::Expr(previous)) =
            result.last_mut().and_then(|item| item.data.as_deref_mut())
        {
            previous.value = join_with_comma(previous.value.clone(), expression.value.clone());
        } else {
            result.push(statement);
        }
    }
    result
}

fn lower_namespace_body(
    core: &mut ParserCore,
    argument: Ref,
    statements: Vec<Stmt>,
    emitted: &mut HashSet<Ref>,
) -> Vec<Stmt> {
    let mut result = Vec::with_capacity(statements.len());
    for statement in statements {
        let loc = statement.loc;
        let Some(data) = statement.data else {
            if !core.options.minify_syntax {
                result.push(statement);
            }
            continue;
        };
        match *data {
            StmtData::Local(local) if local.is_export => {
                lower_namespace_locals(core, argument, loc, local, &mut result);
            }
            StmtData::Function(mut function) if function.is_export => {
                function.is_export = false;
                let name = function.function.name;
                result.push(Stmt::new(loc, StmtData::Function(function)));
                if let Some(name) = name {
                    result.push(namespace_export_assignment(
                        core,
                        argument,
                        name.loc,
                        name.reference,
                    ));
                }
            }
            StmtData::Class(mut class) if class.is_export => {
                if let Some(block_index) = namespace_keep_name_static_block(core, &class) {
                    lower_namespace_keep_name_class(
                        core,
                        argument,
                        loc,
                        class,
                        block_index,
                        &mut result,
                    );
                } else {
                    class.is_export = false;
                    let name = class.class.name;
                    result.push(Stmt::new(loc, StmtData::Class(class)));
                    if let Some(name) = name {
                        result.push(namespace_export_assignment(
                            core,
                            argument,
                            name.loc,
                            name.reference,
                        ));
                    }
                }
            }
            StmtData::Namespace(namespace) => {
                lower_namespace(
                    core,
                    loc,
                    namespace,
                    emitted,
                    &mut result,
                    false,
                    Some(argument),
                );
            }
            StmtData::Enum(enumeration) => {
                lower_enum(
                    core,
                    loc,
                    enumeration,
                    emitted,
                    &mut result,
                    false,
                    Some(argument),
                );
            }
            other => result.push(Stmt::new(loc, other)),
        }
    }
    result
}

fn namespace_keep_name_static_block(core: &ParserCore, class: &ClassStmt) -> Option<usize> {
    if !core.options.keep_names
        || !core
            .options
            .unsupported_js_features
            .contains(JsFeature::CLASS_STATIC_BLOCKS)
    {
        return None;
    }
    class.class.properties.iter().position(|property| {
        if property.kind != PropertyKind::ClassStaticBlock {
            return false;
        }
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
            && matches!(
                call.args.first().and_then(|arg| arg.data.as_deref()),
                Some(ExprData::This)
            )
    })
}

fn lower_namespace_keep_name_class(
    core: &mut ParserCore,
    argument: Ref,
    loc: Loc,
    mut class: ClassStmt,
    block_index: usize,
    result: &mut Vec<Stmt>,
) {
    let outer_name = class
        .class
        .name
        .expect("exported namespace class must have a name");
    let capture_ref = core.new_symbol(
        SymbolKind::Const,
        format!("_{}", symbol_name(core, outer_name.reference)),
    );
    let generated_scope = core.scopes_for_current_part.iter().find_map(|scope| {
        let contains_outer_name = scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .values()
            .any(|member| member.reference == outer_name.reference);
        contains_outer_name.then(|| scope.clone())
    });
    generated_scope
        .or_else(|| core.module_scope.clone())
        .expect("generated class capture requires a scope")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .generated
        .push(capture_ref);
    core.declared_symbols.push(DeclaredSymbol {
        reference: capture_ref,
        is_top_level: false,
    });

    let static_block = class.class.properties.remove(block_index);
    let mut static_statements = static_block
        .class_static_block
        .expect("class static block property")
        .block
        .statements;
    let Some(StmtData::Expr(statement)) = static_statements[0].data.as_deref_mut() else {
        unreachable!("keep-name static block must contain an expression statement");
    };
    let Some(ExprData::Call(call)) = statement.value.data.as_deref_mut() else {
        unreachable!("keep-name static block must contain a call");
    };
    call.args[0] = identifier(call.args[0].loc, capture_ref);
    core.record_usage(capture_ref);

    let mut inner_name = outer_name;
    inner_name.reference = capture_ref;
    class.class.name = Some(inner_name);
    class.is_export = false;
    result.push(Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations: vec![Decl {
                binding: identifier_binding(outer_name.loc, capture_ref),
                value_or_nil: Expr::new(
                    loc,
                    ExprData::Class(ClassExpr {
                        class: class.class,
                        ..ClassExpr::default()
                    }),
                ),
            }],
            kind: LocalKind::Const,
            ..LocalStmt::default()
        }),
    ));
    result.extend(static_statements);

    core.record_usage(capture_ref);
    result.push(Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations: vec![Decl {
                binding: identifier_binding(outer_name.loc, outer_name.reference),
                value_or_nil: identifier(outer_name.loc, capture_ref),
            }],
            kind: LocalKind::Let,
            ..LocalStmt::default()
        }),
    ));
    result.push(namespace_export_assignment_with_value(
        core,
        argument,
        outer_name.loc,
        outer_name.reference,
        capture_ref,
    ));
}

fn namespace_has_runtime_value(statements: &[Stmt]) -> bool {
    statements
        .iter()
        .any(|statement| match statement.data.as_deref() {
            None | Some(StmtData::Empty | StmtData::TypeScript(_) | StmtData::Comment(_)) => false,
            Some(StmtData::Namespace(namespace)) => {
                namespace.has_export_declare || namespace_has_runtime_value(&namespace.statements)
            }
            Some(_) => true,
        })
}

fn lower_namespace_locals(
    core: &mut ParserCore,
    argument: Ref,
    loc: Loc,
    local: LocalStmt,
    result: &mut Vec<Stmt>,
) {
    for declaration in local.declarations {
        let mut binding = declaration.binding;
        for_each_identifier_binding(&mut binding, &mut |_loc, identifier| {
            set_namespace_alias(core, identifier.reference, argument);
            core.record_usage(argument);
        });
        if declaration.value_or_nil.data.is_some() {
            result.push(Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value: assign(
                        loc,
                        convert_binding_to_expr(&binding, None),
                        declaration.value_or_nil,
                    ),
                    ..ExprStmt::default()
                }),
            ));
        }
    }
}

fn namespace_export_assignment(
    core: &mut ParserCore,
    argument: Ref,
    loc: Loc,
    reference: Ref,
) -> Stmt {
    namespace_export_assignment_with_value(core, argument, loc, reference, reference)
}

fn namespace_export_assignment_with_value(
    core: &mut ParserCore,
    argument: Ref,
    loc: Loc,
    property_reference: Ref,
    value_reference: Ref,
) -> Stmt {
    let name = symbol_name(core, property_reference);
    core.record_usage(value_reference);
    Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: assign(
                loc,
                dot(loc, identifier(loc, argument), name),
                identifier(loc, value_reference),
            ),
            ..ExprStmt::default()
        }),
    )
}

fn set_namespace_alias(core: &mut ParserCore, reference: Ref, argument: Ref) {
    let reference = follow_symbols(core, reference);
    let name = core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
        .original_name
        .clone();
    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].namespace_alias =
        Some(NamespaceAlias {
            namespace_ref: argument,
            alias: name,
        });
}

fn namespace_initial_value(
    core: &mut ParserCore,
    loc: Loc,
    name_ref: Ref,
    is_export: bool,
    enclosing_namespace: Option<Ref>,
) -> Expr {
    if is_export && let Some(enclosing_namespace) = enclosing_namespace {
        let property = dot(
            loc,
            identifier(loc, enclosing_namespace),
            symbol_name(core, name_ref),
        );
        if core.options.minify_syntax {
            core.record_usage(enclosing_namespace);
            core.record_usage(name_ref);
            assign(
                loc,
                identifier(loc, name_ref),
                Expr::new(
                    loc,
                    ExprData::Binary(BinaryExpr {
                        left: property,
                        right: Expr::new(loc, ExprData::Object(ObjectExpr::default())),
                        op: OpCode::BinaryLogicalOrAssign,
                    }),
                ),
            )
        } else {
            core.record_usage(enclosing_namespace);
            core.record_usage(enclosing_namespace);
            core.record_usage(name_ref);
            assign(
                loc,
                identifier(loc, name_ref),
                Expr::new(
                    loc,
                    ExprData::Binary(BinaryExpr {
                        left: property.clone(),
                        right: assign(
                            loc,
                            property,
                            Expr::new(loc, ExprData::Object(ObjectExpr::default())),
                        ),
                        op: OpCode::BinaryLogicalOr,
                    }),
                ),
            )
        }
    } else if core.options.minify_syntax {
        core.record_usage(name_ref);
        Expr::new(
            loc,
            ExprData::Binary(BinaryExpr {
                left: identifier(loc, name_ref),
                right: Expr::new(loc, ExprData::Object(ObjectExpr::default())),
                op: OpCode::BinaryLogicalOrAssign,
            }),
        )
    } else {
        core.record_usage(name_ref);
        core.record_usage(name_ref);
        Expr::new(
            loc,
            ExprData::Binary(BinaryExpr {
                left: identifier(loc, name_ref),
                right: assign(
                    loc,
                    identifier(loc, name_ref),
                    Expr::new(loc, ExprData::Object(ObjectExpr::default())),
                ),
                op: OpCode::BinaryLogicalOr,
            }),
        )
    }
}

fn namespace_iife(
    loc: Loc,
    argument: Ref,
    mut body: Vec<Stmt>,
    initial_value: Expr,
    minify_syntax: bool,
) -> Expr {
    let prefer_expr = if minify_syntax && body.len() == 1 {
        if let Some(StmtData::Expr(expression)) =
            body.first().and_then(|statement| statement.data.as_deref())
        {
            let value = expression.value.clone();
            body[0] = Stmt::new(
                loc,
                StmtData::Return(ReturnStmt {
                    value_or_nil: value,
                }),
            );
            true
        } else {
            false
        }
    } else {
        false
    };
    let arrow = Expr::new(
        loc,
        ExprData::Arrow(ArrowExpr {
            args: vec![Arg {
                binding: identifier_binding(loc, argument),
                ..Arg::default()
            }],
            body: FunctionBody {
                block: BlockStmt {
                    statements: body,
                    ..BlockStmt::default()
                },
                loc,
            },
            prefer_expr,
            ..ArrowExpr::default()
        }),
    );
    Expr::new(
        loc,
        ExprData::Call(crate::internal::js_ast::CallExpr {
            target: arrow,
            args: vec![initial_value],
            ..crate::internal::js_ast::CallExpr::default()
        }),
    )
}

fn lower_enum(
    core: &mut ParserCore,
    loc: Loc,
    enumeration: EnumStmt,
    emitted: &mut HashSet<Ref>,
    result: &mut Vec<Stmt>,
    is_module_scope: bool,
    enclosing_namespace: Option<Ref>,
) {
    let name_ref = follow_symbols(core, enumeration.name.reference);
    let is_first_declaration = should_emit_namespace_var(core, name_ref, emitted);
    if !is_module_scope {
        lower_nested_enum(
            core,
            loc,
            enumeration,
            name_ref,
            is_first_declaration,
            result,
            enclosing_namespace,
        );
        return;
    }
    let all_values_are_pure = {
        let helpers = make_helper_context(|reference| {
            let reference = follow_symbols(core, reference);
            core.symbols
                .get(usize::try_from(reference.inner_index).expect("symbol index"))
                .is_none_or(|symbol| symbol.kind == SymbolKind::Unbound)
        });
        enumeration
            .values
            .iter()
            .all(|value| helpers.expr_can_be_removed_if_unused(&value.value_or_nil))
    };
    let mut body = Vec::with_capacity(enumeration.values.len() + 1);
    for value in enumeration.values {
        let argument_use_count =
            if known_primitive_type(value.value_or_nil.data.as_deref()) == PrimitiveType::String {
                1
            } else {
                2
            };
        body.push(lower_enum_value(
            enumeration.argument,
            value.loc,
            value.name,
            value.value_or_nil,
            core.options.minify_syntax,
        ));
        for _ in 0..argument_use_count {
            core.record_usage(enumeration.argument);
        }
    }
    body.push(Stmt::new(
        loc,
        StmtData::Return(ReturnStmt {
            value_or_nil: identifier(loc, enumeration.argument),
        }),
    ));

    let initializer = enum_iife(
        loc,
        enumeration.argument,
        body,
        enum_initial_value(
            core,
            enumeration.name.loc,
            name_ref,
            enumeration.is_export,
            enclosing_namespace,
        ),
        all_values_are_pure,
        core.options.minify_syntax,
    );
    if enclosing_namespace.is_none() || is_first_declaration {
        result.push(Stmt::new(
            loc,
            StmtData::Local(LocalStmt {
                declarations: vec![Decl {
                    binding: identifier_binding(enumeration.name.loc, name_ref),
                    value_or_nil: initializer,
                }],
                kind: if enclosing_namespace.is_some() {
                    LocalKind::Let
                } else {
                    LocalKind::Var
                },
                is_export: enumeration.is_export
                    && enclosing_namespace.is_none()
                    && is_first_declaration,
                ..LocalStmt::default()
            }),
        ));
    } else {
        result.push(Stmt::new(
            loc,
            StmtData::Expr(ExprStmt {
                value: assign(loc, identifier(enumeration.name.loc, name_ref), initializer),
                ..ExprStmt::default()
            }),
        ));
        core.record_usage(name_ref);
    }
    core.record_usage(enumeration.argument);
}

fn lower_nested_enum(
    core: &mut ParserCore,
    loc: Loc,
    enumeration: EnumStmt,
    name_ref: Ref,
    is_first_declaration: bool,
    result: &mut Vec<Stmt>,
    enclosing_namespace: Option<Ref>,
) {
    if is_first_declaration {
        result.push(Stmt::new(
            loc,
            StmtData::Local(LocalStmt {
                declarations: vec![Decl {
                    binding: identifier_binding(enumeration.name.loc, name_ref),
                    ..Decl::default()
                }],
                kind: LocalKind::Let,
                ..LocalStmt::default()
            }),
        ));
    }

    let mut body = Vec::with_capacity(enumeration.values.len());
    for value in enumeration.values {
        let argument_use_count =
            if known_primitive_type(value.value_or_nil.data.as_deref()) == PrimitiveType::String {
                1
            } else {
                2
            };
        body.push(lower_enum_value(
            enumeration.argument,
            value.loc,
            value.name,
            value.value_or_nil,
            core.options.minify_syntax,
        ));
        for _ in 0..argument_use_count {
            core.record_usage(enumeration.argument);
        }
    }
    if core.options.minify_syntax && body.len() > 1 {
        let joined = body
            .into_iter()
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
        body = vec![Stmt::new(
            loc,
            StmtData::Expr(ExprStmt {
                value: joined,
                ..ExprStmt::default()
            }),
        )];
    }

    let initial_value = namespace_initial_value(
        core,
        enumeration.name.loc,
        name_ref,
        enumeration.is_export,
        enclosing_namespace,
    );
    result.push(Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: namespace_iife(
                loc,
                enumeration.argument,
                body,
                initial_value,
                core.options.minify_syntax,
            ),
            ..ExprStmt::default()
        }),
    ));
}

fn lower_enum_value(
    argument: Ref,
    loc: Loc,
    name: Vec<u16>,
    value: Expr,
    minify_syntax: bool,
) -> Stmt {
    let is_string = known_primitive_type(value.data.as_deref()) == PrimitiveType::String;
    let member_name = String::from_utf16_lossy(&name);
    let name = Expr::new(
        loc,
        ExprData::String(StringExpr {
            value: name,
            ..StringExpr::default()
        }),
    );
    let member = if minify_syntax && is_identifier(&member_name) {
        Expr::new(
            loc,
            ExprData::Dot(DotExpr {
                target: identifier(loc, argument),
                name: member_name,
                name_loc: loc,
                ..DotExpr::default()
            }),
        )
    } else {
        Expr::new(
            loc,
            ExprData::Index(IndexExpr {
                target: identifier(loc, argument),
                index: name.clone(),
                ..IndexExpr::default()
            }),
        )
    };
    let assignment = assign(loc, member, value);
    let expression = if is_string {
        assignment
    } else {
        assign(
            loc,
            Expr::new(
                loc,
                ExprData::Index(IndexExpr {
                    target: identifier(loc, argument),
                    index: assignment,
                    ..IndexExpr::default()
                }),
            ),
            name,
        )
    };
    Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: expression,
            ..ExprStmt::default()
        }),
    )
}

fn enum_initial_value(
    core: &mut ParserCore,
    loc: Loc,
    name_ref: Ref,
    is_export: bool,
    enclosing_namespace: Option<Ref>,
) -> Expr {
    if is_export && let Some(enclosing_namespace) = enclosing_namespace {
        let property = dot(
            loc,
            identifier(loc, enclosing_namespace),
            symbol_name(core, name_ref),
        );
        core.record_usage(enclosing_namespace);
        core.record_usage(enclosing_namespace);
        Expr::new(
            loc,
            ExprData::Binary(BinaryExpr {
                left: property.clone(),
                right: assign(
                    loc,
                    property,
                    Expr::new(loc, ExprData::Object(ObjectExpr::default())),
                ),
                op: OpCode::BinaryLogicalOr,
            }),
        )
    } else {
        core.record_usage(name_ref);
        Expr::new(
            loc,
            ExprData::Binary(BinaryExpr {
                left: identifier(loc, name_ref),
                right: Expr::new(loc, ExprData::Object(ObjectExpr::default())),
                op: OpCode::BinaryLogicalOr,
            }),
        )
    }
}

fn enum_iife(
    loc: Loc,
    argument: Ref,
    mut body: Vec<Stmt>,
    initial_value: Expr,
    can_be_unwrapped_if_unused: bool,
    minify_syntax: bool,
) -> Expr {
    if minify_syntax
        && let Some(StmtData::Return(return_statement)) =
            body.last().and_then(|statement| statement.data.as_deref())
        && return_statement.value_or_nil.data.is_some()
        && body[..body.len() - 1]
            .iter()
            .all(|statement| matches!(statement.data.as_deref(), Some(StmtData::Expr(_))))
    {
        let mut combined = body[..body.len() - 1]
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
        body = vec![Stmt::new(
            loc,
            StmtData::Return(ReturnStmt {
                value_or_nil: combined,
            }),
        )];
    }
    let arrow = Expr::new(
        loc,
        ExprData::Arrow(ArrowExpr {
            args: vec![Arg {
                binding: identifier_binding(loc, argument),
                ..Arg::default()
            }],
            body: FunctionBody {
                block: BlockStmt {
                    statements: body,
                    ..BlockStmt::default()
                },
                loc,
            },
            prefer_expr: minify_syntax,
            ..ArrowExpr::default()
        }),
    );
    Expr::new(
        loc,
        ExprData::Call(crate::internal::js_ast::CallExpr {
            target: arrow,
            args: vec![initial_value],
            can_be_unwrapped_if_unused,
            ..crate::internal::js_ast::CallExpr::default()
        }),
    )
}

fn follow_symbols(core: &ParserCore, mut reference: Ref) -> Ref {
    while reference != INVALID_REF {
        let link = core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].link;
        if link == INVALID_REF {
            break;
        }
        reference = link;
    }
    reference
}

fn should_emit_namespace_var(
    core: &ParserCore,
    reference: Ref,
    emitted: &mut HashSet<Ref>,
) -> bool {
    emitted.insert(reference)
        && matches!(
            core.symbols[usize::try_from(reference.inner_index).expect("symbol index")].kind,
            SymbolKind::TsEnum | SymbolKind::TsNamespace
        )
}

fn identifier(loc: Loc, reference: Ref) -> Expr {
    Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }),
    )
}

fn identifier_binding(loc: Loc, reference: Ref) -> Binding {
    Binding {
        loc,
        data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
            reference,
        }))),
    }
}

fn dot(loc: Loc, target: Expr, name: String) -> Expr {
    Expr::new(
        loc,
        ExprData::Dot(DotExpr {
            target,
            name,
            name_loc: loc,
            ..DotExpr::default()
        }),
    )
}

fn symbol_name(core: &ParserCore, reference: Ref) -> String {
    let reference = follow_symbols(core, reference);
    core.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
        .original_name
        .clone()
}

fn assign(loc: Loc, left: Expr, right: Expr) -> Expr {
    Expr::new(
        loc,
        ExprData::Binary(BinaryExpr {
            left,
            right,
            op: OpCode::BinaryAssign,
        }),
    )
}
