#![allow(dead_code)]

use std::collections::HashMap;

use crate::internal::{
    ast::LocRef,
    js_ast::{Arg, BinaryExpr, BindingData, BlockStmt, Expr, ExprData, LocalKind, Stmt, StmtData},
    logger::{Loc, StringInJsTableEntry, remap_string_in_js_loc},
};

use super::control_flow::stmt_cares_about_scope;

pub(crate) fn define_value_can_be_used_in_assign_target(data: Option<&ExprData>) -> bool {
    matches!(data, Some(ExprData::Identifier(_) | ExprData::Dot(_)))
}

pub(crate) fn tag_or_fragment_help_text(tag: &str) -> String {
    if tag.is_empty() {
        "fragment tag".into()
    } else {
        format!("{tag:?} tag")
    }
}

pub(crate) fn stmts_to_single_stmt(loc: Loc, statements: Vec<Stmt>, close_brace_loc: Loc) -> Stmt {
    if statements.is_empty() {
        return Stmt::new(loc, StmtData::Empty);
    }
    if statements.len() == 1 && !stmt_cares_about_scope(&statements[0]) {
        return statements.into_iter().next().expect("length was checked");
    }
    Stmt::new(
        loc,
        StmtData::Block(BlockStmt {
            statements,
            close_brace_loc,
        }),
    )
}

pub(crate) fn try_to_inline_case_body(
    open_brace_loc: Loc,
    mut statements: Vec<Stmt>,
    close_brace_loc: Loc,
) -> Option<Vec<Stmt>> {
    if statements.len() == 1
        && let Some(StmtData::Block(block)) = statements[0].data.as_deref()
    {
        return try_to_inline_case_body(
            statements[0].loc,
            block.statements.clone(),
            block.close_brace_loc,
        );
    }

    let mut cares_about_scope = false;
    let mut truncate = None;
    for (index, statement) in statements.iter().enumerate() {
        match statement.data.as_deref() {
            Some(
                StmtData::Empty
                | StmtData::Directive(_)
                | StmtData::Comment(_)
                | StmtData::Expr(_)
                | StmtData::Debugger
                | StmtData::Continue(_)
                | StmtData::Return(_)
                | StmtData::Throw(_),
            ) => {}
            Some(StmtData::Local(local)) => {
                if local.kind != LocalKind::Var {
                    cares_about_scope = true;
                }
            }
            Some(StmtData::Break(break_stmt)) => {
                if break_stmt.label.is_some() {
                    return None;
                }
                truncate = Some(index);
                break;
            }
            _ => return None,
        }
    }
    if let Some(index) = truncate {
        statements.truncate(index);
    }

    if cares_about_scope {
        Some(vec![Stmt::new(
            open_brace_loc,
            StmtData::Block(BlockStmt {
                statements,
                close_brace_loc,
            }),
        )])
    } else {
        Some(statements)
    }
}

pub(crate) fn is_unsightly_primitive(data: Option<&ExprData>) -> bool {
    matches!(
        data,
        Some(
            ExprData::Boolean(_)
                | ExprData::Null
                | ExprData::Undefined
                | ExprData::Number(_)
                | ExprData::BigInt(_)
                | ExprData::String(_)
        )
    )
}

pub(crate) fn is_safe_for_const_local_prefix(expr: &Expr) -> bool {
    match expr.data.as_deref() {
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

pub(crate) fn is_simple_parameter_list(args: &[Arg], has_rest_arg: bool) -> bool {
    !has_rest_arg
        && args.iter().all(|arg| {
            matches!(
                arg.binding.data.as_deref(),
                Some(BindingData::Identifier(_))
            ) && arg.default_or_nil.data.is_none()
        })
}

pub(crate) fn fn_body_contains_use_strict(body: &[Stmt]) -> Option<Loc> {
    for statement in body {
        match statement.data.as_deref() {
            Some(StmtData::Comment(_)) => {}
            Some(StmtData::Directive(directive))
                if directive.value == "use strict".encode_utf16().collect::<Vec<_>>() =>
            {
                return Some(statement.loc);
            }
            _ => return None,
        }
    }
    None
}

pub(crate) fn loc_after_op(binary: &BinaryExpr) -> Loc {
    if binary.left.loc.start < binary.right.loc.start {
        binary.right.loc
    } else {
        binary.left.loc
    }
}

pub(crate) fn is_eval_or_arguments(name: &str) -> bool {
    matches!(name, "eval" | "arguments")
}

pub(crate) fn contains_closing_script_tag(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .windows(8)
        .any(|window| window[..2] == *b"</" && window[2..].eq_ignore_ascii_case(b"script"))
}

pub(crate) fn sorted_keys_of_map_string_loc_ref(input: &HashMap<String, LocRef>) -> Vec<String> {
    let mut keys = input.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}

pub(crate) fn remap_expr_locs_in_json(expr: &mut Expr, table: &[StringInJsTableEntry]) {
    expr.loc = remap_string_in_js_loc(table, expr.loc);
    match expr.data.as_deref_mut() {
        Some(ExprData::Array(array)) => {
            array.close_bracket_loc = remap_string_in_js_loc(table, array.close_bracket_loc);
            for item in &mut array.items {
                remap_expr_locs_in_json(item, table);
            }
        }
        Some(ExprData::Object(object)) => {
            object.close_brace_loc = remap_string_in_js_loc(table, object.close_brace_loc);
            for property in &mut object.properties {
                remap_expr_locs_in_json(&mut property.key, table);
                remap_expr_locs_in_json(&mut property.value_or_nil, table);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        contains_closing_script_tag, fn_body_contains_use_strict, is_safe_for_const_local_prefix,
        is_simple_parameter_list, remap_expr_locs_in_json, sorted_keys_of_map_string_loc_ref,
        stmts_to_single_stmt, try_to_inline_case_body,
    };
    use crate::internal::{
        ast::{LocRef, Ref},
        js_ast::{
            Arg, ArrayExpr, Binding, BindingData, BreakStmt, CommentStmt, DirectiveStmt, Expr,
            ExprData, IdentifierBinding, LocalKind, LocalStmt, Stmt, StmtData, StringExpr,
        },
        logger::{Loc, StringInJsTableEntry},
    };

    fn stmt(data: StmtData) -> Stmt {
        Stmt::new(Loc::default(), data)
    }

    #[test]
    fn switch_case_inline_removes_unlabeled_break_and_preserves_lexical_scope() {
        let statements = vec![
            stmt(StmtData::Local(LocalStmt {
                kind: LocalKind::Let,
                ..LocalStmt::default()
            })),
            stmt(StmtData::Break(BreakStmt::default())),
        ];
        let result =
            try_to_inline_case_body(Loc { start: 1 }, statements, Loc { start: 2 }).unwrap();
        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0].data.as_deref(),
            Some(StmtData::Block(_))
        ));

        let labeled = vec![stmt(StmtData::Break(BreakStmt {
            label: Some(LocRef::default()),
        }))];
        assert!(try_to_inline_case_body(Loc::default(), labeled, Loc::default()).is_none());
    }

    #[test]
    fn single_statement_conversion_preserves_scope_when_needed() {
        let lexical = stmt(StmtData::Local(LocalStmt {
            kind: LocalKind::Const,
            ..LocalStmt::default()
        }));
        let result = stmts_to_single_stmt(Loc::default(), vec![lexical], Loc { start: 5 });
        assert!(matches!(result.data.as_deref(), Some(StmtData::Block(_))));
    }

    #[test]
    fn checks_simple_parameters_and_safe_const_prefixes() {
        let argument = Arg {
            binding: Binding {
                data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                    reference: Ref::default(),
                }))),
                ..Binding::default()
            },
            ..Arg::default()
        };
        assert!(is_simple_parameter_list(&[argument], false));
        assert!(!is_simple_parameter_list(&[], true));

        let safe = Expr::new(
            Loc::default(),
            ExprData::Array(ArrayExpr {
                items: vec![Expr::new(
                    Loc::default(),
                    ExprData::String(StringExpr::default()),
                )],
                ..ArrayExpr::default()
            }),
        );
        assert!(is_safe_for_const_local_prefix(&safe));
        assert!(!is_safe_for_const_local_prefix(&Expr::new(
            Loc::default(),
            ExprData::Number(1.0)
        )));
    }

    #[test]
    fn detects_use_strict_after_comments_only() {
        let body = [
            stmt(StmtData::Comment(CommentStmt::default())),
            Stmt::new(
                Loc { start: 3 },
                StmtData::Directive(DirectiveStmt {
                    value: "use strict".encode_utf16().collect(),
                    ..DirectiveStmt::default()
                }),
            ),
        ];
        assert_eq!(fn_body_contains_use_strict(&body), Some(Loc { start: 3 }));
    }

    #[test]
    fn closing_script_detection_is_ascii_case_insensitive_and_keys_sort() {
        assert!(contains_closing_script_tag("x</ScRiPt anything"));
        assert!(!contains_closing_script_tag("</style>"));

        let mut map = HashMap::new();
        map.insert("z".into(), LocRef::default());
        map.insert("a".into(), LocRef::default());
        assert_eq!(sorted_keys_of_map_string_loc_ref(&map), ["a", "z"]);
    }

    #[test]
    fn remaps_nested_json_expression_locations() {
        let mut expr = Expr::new(
            Loc { start: 1 },
            ExprData::Array(ArrayExpr {
                items: vec![Expr::new(Loc { start: 2 }, ExprData::Number(1.0))],
                close_bracket_loc: Loc { start: 3 },
                ..ArrayExpr::default()
            }),
        );
        let table = [StringInJsTableEntry {
            inner_loc: Loc { start: 0 },
            outer_loc: Loc { start: 10 },
            ..StringInJsTableEntry::default()
        }];
        remap_expr_locs_in_json(&mut expr, &table);
        assert_eq!(expr.loc.start, 11);
        let Some(ExprData::Array(array)) = expr.data.as_deref() else {
            panic!("expected array");
        };
        assert_eq!(array.items[0].loc.start, 12);
        assert_eq!(array.close_bracket_loc.start, 13);
    }
}
