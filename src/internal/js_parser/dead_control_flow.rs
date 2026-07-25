#![allow(dead_code)]

use crate::internal::js_ast::{Binding, BindingData, Decl, LocalKind, Stmt, StmtData};

pub(crate) fn find_identifiers(binding: &Binding, identifiers: &mut Vec<Decl>) {
    match binding.data.as_deref() {
        Some(BindingData::Identifier(_)) => identifiers.push(Decl {
            binding: binding.clone(),
            ..Decl::default()
        }),
        Some(BindingData::Array(array)) => {
            for item in &array.items {
                find_identifiers(&item.binding, identifiers);
            }
        }
        Some(BindingData::Object(object)) => {
            for property in &object.properties {
                find_identifiers(&property.value, identifiers);
            }
        }
        Some(BindingData::Missing) | None => {}
    }
}

pub(crate) fn should_keep_stmts_in_dead_control_flow(statements: &mut [Stmt]) -> bool {
    statements
        .iter_mut()
        .any(should_keep_stmt_in_dead_control_flow)
}

pub(crate) fn should_keep_stmt_in_dead_control_flow(statement: &mut Stmt) -> bool {
    match statement.data.as_deref_mut() {
        Some(
            StmtData::Empty
            | StmtData::Expr(_)
            | StmtData::Throw(_)
            | StmtData::Return(_)
            | StmtData::Break(_)
            | StmtData::Continue(_)
            | StmtData::Class(_)
            | StmtData::Debugger,
        )
        | None => false,

        Some(StmtData::Local(local)) => {
            if local.kind != LocalKind::Var {
                return false;
            }

            let mut identifiers = Vec::new();
            for declaration in &local.declarations {
                find_identifiers(&declaration.binding, &mut identifiers);
            }
            if identifiers.is_empty() {
                return false;
            }
            local.declarations = identifiers;
            true
        }

        Some(StmtData::Block(block)) => {
            should_keep_stmts_in_dead_control_flow(&mut block.statements)
        }

        Some(StmtData::Try(try_stmt)) => {
            should_keep_stmts_in_dead_control_flow(&mut try_stmt.block.statements)
                || try_stmt.catch.as_mut().is_some_and(|catch| {
                    should_keep_stmts_in_dead_control_flow(&mut catch.block.statements)
                })
                || try_stmt.finally.as_mut().is_some_and(|finally| {
                    should_keep_stmts_in_dead_control_flow(&mut finally.block.statements)
                })
        }

        Some(StmtData::If(if_stmt)) => {
            should_keep_stmt_in_dead_control_flow(&mut if_stmt.yes)
                || (if_stmt.no_or_nil.data.is_some()
                    && should_keep_stmt_in_dead_control_flow(&mut if_stmt.no_or_nil))
        }

        Some(StmtData::While(while_stmt)) => {
            should_keep_stmt_in_dead_control_flow(&mut while_stmt.body)
        }

        Some(StmtData::DoWhile(do_while)) => {
            should_keep_stmt_in_dead_control_flow(&mut do_while.body)
        }

        Some(StmtData::For(for_stmt)) => {
            (for_stmt.init_or_nil.data.is_some()
                && should_keep_stmt_in_dead_control_flow(&mut for_stmt.init_or_nil))
                || should_keep_stmt_in_dead_control_flow(&mut for_stmt.body)
        }

        Some(StmtData::ForIn(for_in)) => {
            should_keep_stmt_in_dead_control_flow(&mut for_in.init)
                || should_keep_stmt_in_dead_control_flow(&mut for_in.body)
        }

        Some(StmtData::ForOf(for_of)) => {
            should_keep_stmt_in_dead_control_flow(&mut for_of.init)
                || should_keep_stmt_in_dead_control_flow(&mut for_of.body)
        }

        Some(StmtData::Label(label)) => should_keep_stmt_in_dead_control_flow(&mut label.statement),

        Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        find_identifiers, should_keep_stmt_in_dead_control_flow,
        should_keep_stmts_in_dead_control_flow,
    };
    use crate::internal::{
        ast::Ref,
        js_ast::{
            ArrayBinding, ArrayBindingPattern, Binding, BindingData, Decl, Expr, ExprData,
            ExprStmt, IdentifierBinding, LocalKind, LocalStmt, Stmt, StmtData,
        },
        logger::Loc,
    };

    fn identifier(inner_index: u32) -> Binding {
        Binding {
            data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                reference: Ref {
                    source_index: 1,
                    inner_index,
                },
            }))),
            loc: Loc {
                start: i32::try_from(inner_index).unwrap(),
            },
        }
    }

    #[test]
    fn finds_identifiers_inside_destructuring_bindings() {
        let binding = Binding {
            data: Some(Box::new(BindingData::Array(ArrayBindingPattern {
                items: vec![
                    ArrayBinding {
                        binding: identifier(1),
                        ..ArrayBinding::default()
                    },
                    ArrayBinding {
                        binding: identifier(2),
                        ..ArrayBinding::default()
                    },
                ],
                ..ArrayBindingPattern::default()
            }))),
            ..Binding::default()
        };
        let mut identifiers = Vec::new();
        find_identifiers(&binding, &mut identifiers);
        assert_eq!(identifiers.len(), 2);
        assert_eq!(identifiers[0].binding.loc.start, 1);
        assert_eq!(identifiers[1].binding.loc.start, 2);
    }

    #[test]
    fn dead_var_declarations_keep_only_hoisted_identifiers() {
        let binding = Binding {
            data: Some(Box::new(BindingData::Array(ArrayBindingPattern {
                items: vec![
                    ArrayBinding {
                        binding: identifier(1),
                        ..ArrayBinding::default()
                    },
                    ArrayBinding {
                        binding: identifier(2),
                        ..ArrayBinding::default()
                    },
                ],
                ..ArrayBindingPattern::default()
            }))),
            ..Binding::default()
        };
        let mut statement = Stmt::new(
            Loc::default(),
            StmtData::Local(LocalStmt {
                declarations: vec![Decl {
                    binding,
                    value_or_nil: Expr::new(Loc::default(), ExprData::Number(1.0)),
                }],
                kind: LocalKind::Var,
                ..LocalStmt::default()
            }),
        );
        assert!(should_keep_stmt_in_dead_control_flow(&mut statement));
        let Some(StmtData::Local(local)) = statement.data.as_deref() else {
            panic!("expected local statement");
        };
        assert_eq!(local.declarations.len(), 2);
        assert!(
            local
                .declarations
                .iter()
                .all(|declaration| declaration.value_or_nil.data.is_none())
        );
    }

    #[test]
    fn dead_lexical_and_expression_statements_are_removed() {
        let mut lexical = Stmt::new(
            Loc::default(),
            StmtData::Local(LocalStmt {
                declarations: vec![Decl {
                    binding: identifier(1),
                    ..Decl::default()
                }],
                kind: LocalKind::Let,
                ..LocalStmt::default()
            }),
        );
        assert!(!should_keep_stmt_in_dead_control_flow(&mut lexical));

        let mut statements = [Stmt::new(
            Loc::default(),
            StmtData::Expr(ExprStmt::default()),
        )];
        assert!(!should_keep_stmts_in_dead_control_flow(&mut statements));
    }
}
