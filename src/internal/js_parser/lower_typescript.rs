use std::collections::HashSet;

use crate::internal::{
    ast::{INVALID_REF, NamespaceAlias, Ref, SymbolKind},
    js_ast::{
        Arg, ArrowExpr, BinaryExpr, Binding, BindingData, BlockStmt, Decl, DotExpr, EnumStmt, Expr,
        ExprData, ExprStmt, FunctionBody, IdentifierBinding, IdentifierExpr, IndexExpr, LocalKind,
        LocalStmt, NamespaceStmt, ObjectExpr, OpCode, PrimitiveType, ReturnStmt, Stmt, StmtData,
        StringExpr, convert_binding_to_expr, for_each_identifier_binding, known_primitive_type,
        make_helper_context,
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
                result.push(statement);
                continue;
            };
            match *data {
                StmtData::Enum(enumeration) => {
                    lower_enum(core, loc, enumeration, &mut self.emitted, &mut result, None);
                }
                StmtData::Namespace(namespace) => {
                    lower_namespace(core, loc, namespace, &mut self.emitted, &mut result, None);
                }
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
    enclosing_namespace: Option<Ref>,
) {
    if !namespace_has_runtime_value(&namespace.statements) {
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
                kind: if enclosing_namespace.is_some() {
                    LocalKind::Let
                } else {
                    LocalKind::Var
                },
                is_export: namespace.is_export && enclosing_namespace.is_none(),
                ..LocalStmt::default()
            }),
        ));
    }
    let body = lower_namespace_body(core, namespace.argument, namespace.statements, emitted);
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
            value: namespace_iife(loc, namespace.argument, body, initial_value),
            ..ExprStmt::default()
        }),
    ));
    core.record_usage(namespace.argument);
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
            result.push(statement);
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
            StmtData::Namespace(namespace) => {
                lower_namespace(core, loc, namespace, emitted, &mut result, Some(argument));
            }
            StmtData::Enum(enumeration) => {
                lower_enum(core, loc, enumeration, emitted, &mut result, Some(argument));
            }
            other => result.push(Stmt::new(loc, other)),
        }
    }
    result
}

fn namespace_has_runtime_value(statements: &[Stmt]) -> bool {
    statements
        .iter()
        .any(|statement| match statement.data.as_deref() {
            None | Some(StmtData::Empty | StmtData::TypeScript(_) | StmtData::Comment(_)) => false,
            Some(StmtData::Namespace(namespace)) => {
                namespace_has_runtime_value(&namespace.statements)
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
            core.record_usage(identifier.reference);
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
    let name = symbol_name(core, reference);
    core.record_usage(argument);
    core.record_usage(reference);
    Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: assign(
                loc,
                dot(loc, identifier(loc, argument), name),
                identifier(loc, reference),
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
    let target = if is_export && let Some(enclosing_namespace) = enclosing_namespace {
        let property = dot(
            loc,
            identifier(loc, enclosing_namespace),
            symbol_name(core, name_ref),
        );
        core.record_usage(enclosing_namespace);
        core.record_usage(enclosing_namespace);
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
    };
    core.record_usage(name_ref);
    target
}

fn namespace_iife(loc: Loc, argument: Ref, body: Vec<Stmt>, initial_value: Expr) -> Expr {
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
    enclosing_namespace: Option<Ref>,
) {
    let name_ref = follow_symbols(core, enumeration.name.reference);
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
    if should_emit_namespace_var(core, name_ref, emitted) {
        result.push(Stmt::new(
            loc,
            StmtData::Local(LocalStmt {
                declarations: vec![Decl {
                    binding: identifier_binding(enumeration.name.loc, name_ref),
                    ..Decl::default()
                }],
                kind: if enclosing_namespace.is_some() {
                    LocalKind::Let
                } else {
                    LocalKind::Var
                },
                is_export: enumeration.is_export && enclosing_namespace.is_none(),
                ..LocalStmt::default()
            }),
        ));
    }

    let mut body = Vec::with_capacity(enumeration.values.len() + 1);
    for value in enumeration.values {
        body.push(lower_enum_value(
            enumeration.argument,
            value.loc,
            value.name,
            value.value_or_nil,
        ));
        core.record_usage(enumeration.argument);
    }
    body.push(Stmt::new(
        loc,
        StmtData::Return(ReturnStmt {
            value_or_nil: identifier(loc, enumeration.argument),
        }),
    ));

    result.push(Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: assign(
                loc,
                identifier(enumeration.name.loc, name_ref),
                enum_iife(
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
                ),
            ),
            ..ExprStmt::default()
        }),
    ));
    core.record_usage(name_ref);
    core.record_usage(enumeration.argument);
}

fn lower_enum_value(argument: Ref, loc: Loc, name: Vec<u16>, value: Expr) -> Stmt {
    let is_string = known_primitive_type(value.data.as_deref()) == PrimitiveType::String;
    let name = Expr::new(
        loc,
        ExprData::String(StringExpr {
            value: name,
            ..StringExpr::default()
        }),
    );
    let member = Expr::new(
        loc,
        ExprData::Index(IndexExpr {
            target: identifier(loc, argument),
            index: name.clone(),
            ..IndexExpr::default()
        }),
    );
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
    body: Vec<Stmt>,
    initial_value: Expr,
    can_be_unwrapped_if_unused: bool,
) -> Expr {
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
