use std::collections::HashSet;

use crate::internal::{
    ast::{INVALID_REF, Ref},
    js_ast::{
        Arg, ArrowExpr, BinaryExpr, Binding, BindingData, BlockStmt, Decl, EnumStmt, Expr,
        ExprData, ExprStmt, FunctionBody, IdentifierBinding, IdentifierExpr, IndexExpr, LocalKind,
        LocalStmt, ObjectExpr, OpCode, PrimitiveType, ReturnStmt, Stmt, StmtData, StringExpr,
        known_primitive_type,
    },
    logger::Loc,
};

use super::parser_core::ParserCore;

pub(crate) fn lower_type_script_enums(core: &mut ParserCore, statements: Vec<Stmt>) -> Vec<Stmt> {
    let mut emitted = HashSet::new();
    let mut result = Vec::with_capacity(statements.len());
    for statement in statements {
        let loc = statement.loc;
        let Some(data) = statement.data else {
            result.push(statement);
            continue;
        };
        match *data {
            StmtData::Enum(enumeration) => {
                lower_enum(core, loc, enumeration, &mut emitted, &mut result);
            }
            other => result.push(Stmt::new(loc, other)),
        }
    }
    result
}

fn lower_enum(
    core: &mut ParserCore,
    loc: Loc,
    enumeration: EnumStmt,
    emitted: &mut HashSet<Ref>,
    result: &mut Vec<Stmt>,
) {
    let name_ref = follow_symbols(core, enumeration.name.reference);
    if emitted.insert(name_ref) {
        result.push(Stmt::new(
            loc,
            StmtData::Local(LocalStmt {
                declarations: vec![Decl {
                    binding: identifier_binding(enumeration.name.loc, name_ref),
                    ..Decl::default()
                }],
                kind: LocalKind::Var,
                is_export: enumeration.is_export,
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
                enum_iife(loc, name_ref, enumeration.argument, body),
            ),
            ..ExprStmt::default()
        }),
    ));
    core.record_usage(name_ref);
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

fn enum_iife(loc: Loc, name_ref: Ref, argument: Ref, body: Vec<Stmt>) -> Expr {
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
    let initial_value = Expr::new(
        loc,
        ExprData::Binary(BinaryExpr {
            left: identifier(loc, name_ref),
            right: Expr::new(loc, ExprData::Object(ObjectExpr::default())),
            op: OpCode::BinaryLogicalOr,
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
