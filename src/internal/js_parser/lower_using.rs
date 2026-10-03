use std::sync::{Arc, Weak};

use crate::internal::{
    ast::{LocRef, Ref, SymbolKind},
    compat::JsFeature,
    js_ast::{
        ArrayExpr, BinaryExpr, Binding, BindingData, BlockStmt, Catch, ClauseItem, Decl,
        DeclaredSymbol, ExportClauseStmt, Expr, ExprData, ExprStmt, Finally, IdentifierBinding,
        IdentifierExpr, LocalKind, LocalStmt, OpCode, Stmt, StmtData, TryStmt,
        for_each_identifier_binding,
    },
    logger::Loc,
};

use super::{parser_core::ParserCore, symbols::select_local_kind};

pub(super) fn should_lower_kind(core: &ParserCore, kind: LocalKind) -> bool {
    let unsupported = core.options.unsupported_js_features;
    match kind {
        LocalKind::Using => unsupported.contains(JsFeature::USING),
        LocalKind::AwaitUsing => {
            unsupported.contains(JsFeature::USING)
                || unsupported.contains(JsFeature::ASYNC_AWAIT)
                || (core.visit_is_generator && unsupported.contains(JsFeature::ASYNC_GENERATOR))
        }
        _ => false,
    }
}

pub(super) fn lower_statements(core: &mut ParserCore, statements: &mut Vec<Stmt>) {
    if statements.iter().any(|statement| {
        matches!(statement.data.as_deref(),
        Some(StmtData::Local(local)) if should_lower_kind(core, local.kind))
    }) {
        if core.is_current_scope_module_scope() {
            *statements = super::lower_typescript::lower_type_script_statements(
                core,
                std::mem::take(statements),
            );
        } else {
            super::lower_typescript::lower_nested_type_script_statements(core, statements, None);
        }
        let mut context = UsingContext::new(core);
        context.scan(core, statements);
        let hoist = core.is_current_scope_module_scope();
        *statements = context.finalize(core, std::mem::take(statements), hoist);
    }
}

pub(super) fn lower_for_of(core: &mut ParserCore, loc: Loc, init: &mut Stmt, body: &mut Stmt) {
    let Some(StmtData::Local(local)) = init.data.as_deref_mut() else {
        return;
    };
    if !should_lower_kind(core, local.kind) {
        return;
    }
    let declaration = &mut local.declarations[0];
    let binding = declaration.binding.clone();
    let Some(BindingData::Identifier(identifier)) = binding.data.as_deref() else {
        return;
    };
    let name = format!(
        "_{}",
        core.symbols[identifier.reference.inner_index as usize].original_name
    );
    let temporary = core.new_symbol(SymbolKind::Other, name);
    core.current_scope
        .as_ref()
        .expect("for-of scope")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .generated
        .push(temporary);
    core.record_declared_symbol(temporary);
    let mut statements = vec![Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations: vec![Decl {
                binding,
                value_or_nil: usage(core, loc, temporary),
            }],
            kind: local.kind,
            ..LocalStmt::default()
        }),
    )];
    match std::mem::take(body).data.map(|data| *data) {
        Some(StmtData::Block(block)) => statements.extend(block.statements),
        Some(StmtData::Empty) | None => {}
        Some(data) => statements.push(Stmt::new(loc, data)),
    }
    let mut context = UsingContext::new(core);
    context.scan(core, &mut statements);
    *body = Stmt::new(
        loc,
        StmtData::Block(BlockStmt {
            statements: context.finalize(core, statements, false),
            ..BlockStmt::default()
        }),
    );
    local.kind = LocalKind::Var;
    declaration.binding = identifier_binding(declaration.binding.loc, temporary);
}

fn identifier_binding(loc: Loc, reference: Ref) -> Binding {
    Binding {
        loc,
        data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
            reference,
        }))),
    }
}

fn usage(core: &mut ParserCore, loc: Loc, reference: Ref) -> Expr {
    core.record_usage(reference);
    Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }),
    )
}

fn local(loc: Loc, declarations: Vec<Decl>) -> Stmt {
    Stmt::new(
        loc,
        StmtData::Local(LocalStmt {
            declarations,
            ..LocalStmt::default()
        }),
    )
}

struct UsingContext {
    first_loc: Loc,
    stack: Ref,
    has_await: bool,
}

impl UsingContext {
    fn new(core: &mut ParserCore) -> Self {
        Self {
            first_loc: Loc::default(),
            stack: core.new_symbol(SymbolKind::Other, "_stack"),
            has_await: false,
        }
    }

    fn scan(&mut self, core: &mut ParserCore, statements: &mut [Stmt]) {
        for statement in statements {
            let Some(StmtData::Local(local)) = statement.data.as_deref_mut() else {
                continue;
            };
            if !matches!(local.kind, LocalKind::Using | LocalKind::AwaitUsing) {
                continue;
            }
            if self.first_loc.start == 0 {
                self.first_loc = statement.loc;
            }
            let is_async = local.kind == LocalKind::AwaitUsing;
            self.has_await |= is_async;
            for declaration in &mut local.declarations {
                if declaration.value_or_nil.data.is_none() {
                    continue;
                }
                let value = std::mem::take(&mut declaration.value_or_nil);
                let loc = value.loc;
                let mut arguments = vec![usage(core, loc, self.stack), value];
                if is_async {
                    arguments.push(Expr::new(loc, ExprData::Boolean(true)));
                }
                declaration.value_or_nil = core.call_runtime(loc, "__using", arguments);
            }
            local.kind = select_local_kind(
                LocalKind::Const,
                &core.options,
                core.is_current_scope_module_scope(),
                core.will_wrap_module_in_try_catch_for_using,
            );
        }
    }

    fn finalize(
        self,
        core: &mut ParserCore,
        statements: Vec<Stmt>,
        hoist_functions: bool,
    ) -> Vec<Stmt> {
        let (mut result, statements, exports) =
            filter_statements(core, statements, hoist_functions);
        let caught = core.new_symbol(SymbolKind::Other, "_");
        let error = core.new_symbol(SymbolKind::Other, "_error");
        let has_error = core.new_symbol(SymbolKind::Other, "_hasError");
        register_generated(core, &[self.stack, caught, error, has_error]);
        let loc = self.first_loc;
        let arguments = vec![
            usage(core, loc, self.stack),
            usage(core, loc, error),
            usage(core, loc, has_error),
        ];
        let call_dispose = core.call_runtime(loc, "__callDispose", arguments);
        let finally = if self.has_await {
            let promise = core.new_symbol(SymbolKind::Other, "_promise");
            register_generated(core, &[promise]);
            let value = usage(core, loc, promise);
            let awaited = super::visit::lower_await_value(core, loc, value);
            let condition = usage(core, loc, promise);
            vec![
                local(
                    loc,
                    vec![Decl {
                        binding: identifier_binding(loc, promise),
                        value_or_nil: call_dispose,
                    }],
                ),
                Stmt::new(
                    loc,
                    StmtData::Expr(ExprStmt {
                        value: Expr::new(
                            loc,
                            ExprData::Binary(BinaryExpr {
                                op: OpCode::BinaryLogicalAnd,
                                left: condition,
                                right: awaited,
                            }),
                        ),
                        ..ExprStmt::default()
                    }),
                ),
            ]
        } else {
            vec![Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value: call_dispose,
                    ..ExprStmt::default()
                }),
            )]
        };
        let catch = disposal_catch(core, loc, caught, error, has_error);
        result.push(local(
            loc,
            vec![Decl {
                binding: identifier_binding(loc, self.stack),
                value_or_nil: Expr::new(loc, ExprData::Array(ArrayExpr::default())),
            }],
        ));
        result.push(Stmt::new(
            loc,
            StmtData::Try(TryStmt {
                block: BlockStmt {
                    statements,
                    ..BlockStmt::default()
                },
                block_loc: loc,
                catch: Some(catch),
                finally: Some(Finally {
                    loc,
                    block: BlockStmt {
                        statements: finally,
                        ..BlockStmt::default()
                    },
                }),
            }),
        ));
        if !exports.is_empty() {
            result.push(Stmt::new(
                loc,
                StmtData::ExportClause(ExportClauseStmt {
                    items: exports,
                    ..ExportClauseStmt::default()
                }),
            ));
        }
        result
    }
}

fn disposal_catch(
    core: &mut ParserCore,
    loc: Loc,
    caught: Ref,
    error: Ref,
    has_error: Ref,
) -> Catch {
    Catch {
        binding_or_nil: identifier_binding(loc, caught),
        block: BlockStmt {
            statements: vec![local(
                loc,
                vec![
                    Decl {
                        binding: identifier_binding(loc, error),
                        value_or_nil: usage(core, loc, caught),
                    },
                    Decl {
                        binding: identifier_binding(loc, has_error),
                        value_or_nil: Expr::new(loc, ExprData::Boolean(true)),
                    },
                ],
            )],
            ..BlockStmt::default()
        },
        loc,
        block_loc: loc,
    }
}

fn register_generated(core: &mut ParserCore, references: &[Ref]) {
    let mut scope = core.current_scope.as_ref().expect("using scope").clone();
    loop {
        let (kind, parent) = {
            let scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (scope.kind, scope.parent.as_ref().and_then(Weak::upgrade))
        };
        if kind.stops_hoisting() {
            break;
        }
        scope = parent.expect("using hoist scope");
    }
    let is_top_level = core
        .module_scope
        .as_ref()
        .is_some_and(|module| Arc::ptr_eq(module, &scope));
    scope
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .generated
        .extend(references);
    core.declared_symbols
        .extend(references.iter().map(|&reference| DeclaredSymbol {
            reference,
            is_top_level,
        }));
}

fn filter_statements(
    core: &ParserCore,
    statements: Vec<Stmt>,
    hoist_functions: bool,
) -> (Vec<Stmt>, Vec<Stmt>, Vec<ClauseItem>) {
    let mut outside = Vec::new();
    let mut inside = Vec::new();
    let mut exports = Vec::new();
    for mut statement in statements {
        match statement.data.as_deref_mut() {
            Some(
                StmtData::Directive(_)
                | StmtData::Import(_)
                | StmtData::ExportFrom(_)
                | StmtData::ExportStar(_),
            ) => {
                outside.push(statement);
                continue;
            }
            Some(StmtData::ExportClause(clause)) => {
                exports.append(&mut clause.items);
                continue;
            }
            Some(StmtData::Function(_)) if hoist_functions => {
                outside.push(statement);
                continue;
            }
            Some(StmtData::ExportDefault(export))
                if hoist_functions
                    && matches!(export.value.data.as_deref(), Some(StmtData::Function(_))) =>
            {
                outside.push(statement);
                continue;
            }
            Some(StmtData::ExportDefault(export)) => {
                if let Some(StmtData::Expr(value)) = export.value.data.as_deref_mut() {
                    let reference = export.default_name.reference;
                    exports.push(ClauseItem {
                        alias: "default".into(),
                        name: export.default_name,
                        alias_loc: statement.loc,
                        ..ClauseItem::default()
                    });
                    statement = local(
                        statement.loc,
                        vec![Decl {
                            binding: identifier_binding(export.default_name.loc, reference),
                            value_or_nil: std::mem::take(&mut value.value),
                        }],
                    );
                }
            }
            Some(StmtData::Local(local)) if local.is_export => {
                for declaration in &mut local.declarations {
                    for_each_identifier_binding(
                        &mut declaration.binding,
                        &mut |loc, identifier| {
                            exports.push(ClauseItem {
                                alias: core.symbols[identifier.reference.inner_index as usize]
                                    .original_name
                                    .clone(),
                                name: LocRef {
                                    loc,
                                    reference: identifier.reference,
                                },
                                alias_loc: loc,
                                ..ClauseItem::default()
                            });
                        },
                    );
                }
                local.kind = LocalKind::Var;
                local.is_export = false;
            }
            _ => {}
        }
        inside.push(statement);
    }
    (outside, inside, exports)
}
