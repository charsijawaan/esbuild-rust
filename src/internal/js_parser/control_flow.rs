#![allow(dead_code)]

use crate::internal::{
    helpers::{hash_combine, hash_combine_string},
    js_ast::{
        BinaryExpr, EqualityKind, Expr, ExprData, ForStmt, LocalKind, OpCode, Stmt, StmtData,
        SwitchStmt, check_equality_big_int, check_equality_if_no_side_effects,
    },
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(i8)]
pub(crate) enum LivenessStatus {
    AlwaysDead = -1,
    Unknown = 0,
    AlwaysLive = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SwitchCaseLiveness {
    pub(crate) status: LivenessStatus,
    pub(crate) can_fall_through: bool,
}

pub(crate) fn analyze_switch_cases_for_liveness(switch: &SwitchStmt) -> Vec<SwitchCaseLiveness> {
    let mut cases = Vec::with_capacity(switch.cases.len());
    let mut default_index = None;

    let mut max_status = LivenessStatus::AlwaysDead;
    for (index, case) in switch.cases.iter().enumerate() {
        if case.value_or_nil.data.is_none() {
            default_index = Some(index);
        }

        let status = if max_status == LivenessStatus::AlwaysLive || case.value_or_nil.data.is_none()
        {
            LivenessStatus::AlwaysDead
        } else {
            match check_equality_if_no_side_effects(
                switch.test.data.as_deref(),
                case.value_or_nil.data.as_deref(),
                EqualityKind::Strict,
            ) {
                Some(true) => LivenessStatus::AlwaysLive,
                Some(false) => LivenessStatus::AlwaysDead,
                None => LivenessStatus::Unknown,
            }
        };
        max_status = max_status.max(status);
        cases.push(SwitchCaseLiveness {
            status,
            can_fall_through: case_body_could_have_fall_through(&case.body),
        });
    }

    if let Some(default_index) = default_index {
        let status = match max_status {
            LivenessStatus::AlwaysDead => LivenessStatus::AlwaysLive,
            LivenessStatus::Unknown => LivenessStatus::Unknown,
            LivenessStatus::AlwaysLive => LivenessStatus::AlwaysDead,
        };
        max_status = max_status.max(status);
        cases[default_index].status = status;
    }

    for index in 0..cases.len() {
        if cases[index].status != LivenessStatus::AlwaysDead {
            let mut next = index + 1;
            while next < cases.len() && cases[next - 1].can_fall_through {
                cases[next].status = LivenessStatus::Unknown;
                next += 1;
            }
        } else if max_status > LivenessStatus::AlwaysDead
            && stmts_care_about_scope(&switch.cases[index].body)
        {
            cases[index].status = LivenessStatus::Unknown;
        }
    }
    cases
}

pub(crate) fn case_body_could_have_fall_through(mut statements: &[Stmt]) -> bool {
    while let Some(statement) = statements.last() {
        match statement.data.as_deref() {
            Some(StmtData::Block(block)) => statements = &block.statements,
            Some(
                StmtData::Break(_)
                | StmtData::Continue(_)
                | StmtData::Return(_)
                | StmtData::Throw(_),
            ) => return false,
            _ => break,
        }
    }
    true
}

pub(crate) fn stmt_cares_about_scope(statement: &Stmt) -> bool {
    match statement.data.as_deref() {
        Some(
            StmtData::Block(_)
            | StmtData::Empty
            | StmtData::Debugger
            | StmtData::Expr(_)
            | StmtData::If(_)
            | StmtData::For(_)
            | StmtData::ForIn(_)
            | StmtData::ForOf(_)
            | StmtData::DoWhile(_)
            | StmtData::While(_)
            | StmtData::With(_)
            | StmtData::Try(_)
            | StmtData::Switch(_)
            | StmtData::Return(_)
            | StmtData::Throw(_)
            | StmtData::Break(_)
            | StmtData::Continue(_)
            | StmtData::Directive(_)
            | StmtData::Label(_),
        )
        | None => false,
        Some(StmtData::Local(local)) => local.kind != LocalKind::Var,
        Some(_) => true,
    }
}

pub(crate) fn stmts_care_about_scope(statements: &[Stmt]) -> bool {
    statements.iter().any(stmt_cares_about_scope)
}

pub(crate) fn is_jump_statement(statement: &StmtData) -> bool {
    matches!(
        statement,
        StmtData::Break(_) | StmtData::Continue(_) | StmtData::Return(_) | StmtData::Throw(_)
    )
}

pub(crate) fn jump_stmts_look_the_same(left: &StmtData, right: &StmtData) -> bool {
    match (left, right) {
        (StmtData::Break(left), StmtData::Break(right)) => {
            left.label.as_ref().map(|label| label.reference)
                == right.label.as_ref().map(|label| label.reference)
        }
        (StmtData::Continue(left), StmtData::Continue(right)) => {
            left.label.as_ref().map(|label| label.reference)
                == right.label.as_ref().map(|label| label.reference)
        }
        (StmtData::Return(left), StmtData::Return(right)) => {
            match (
                left.value_or_nil.data.as_deref(),
                right.value_or_nil.data.as_deref(),
            ) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    crate::internal::js_ast::values_look_the_same(Some(left), Some(right))
                }
                _ => false,
            }
        }
        (StmtData::Throw(left), StmtData::Throw(right)) => {
            crate::internal::js_ast::values_look_the_same(
                left.value.data.as_deref(),
                right.value.data.as_deref(),
            )
        }
        _ => false,
    }
}

pub(crate) fn drop_first_statement(mut body: Stmt, replacement: Stmt) -> Stmt {
    if let Some(StmtData::Block(block)) = body.data.as_deref_mut()
        && !block.statements.is_empty()
    {
        if replacement.data.is_some() {
            block.statements[0] = replacement;
        } else if block.statements.len() == 2 && !stmt_cares_about_scope(&block.statements[1]) {
            return block.statements[1].clone();
        } else {
            block.statements.remove(0);
        }
        return body;
    }
    if replacement.data.is_some() {
        return replacement;
    }
    Stmt::new(body.loc, StmtData::Empty)
}

pub(crate) fn mangle_for(for_stmt: &mut ForStmt) {
    let mut first = &for_stmt.body;
    if let Some(StmtData::Block(block)) = first.data.as_deref()
        && let Some(statement) = block.statements.first()
    {
        first = statement;
    }
    let Some(StmtData::If(if_stmt)) = first.data.as_deref() else {
        return;
    };
    let if_stmt = if_stmt.clone();

    if matches!(
        if_stmt.yes.data.as_deref(),
        Some(StmtData::Break(break_stmt)) if break_stmt.label.is_none()
    ) {
        let inverted = match if_stmt.test.data.as_deref() {
            Some(ExprData::Unary(unary)) if unary.op == OpCode::UnaryNot => unary.value.clone(),
            _ => crate::internal::js_ast::not(if_stmt.test),
        };
        for_stmt.test_or_nil = join_loop_test(for_stmt.test_or_nil.clone(), inverted);
        for_stmt.body = drop_first_statement(for_stmt.body.clone(), if_stmt.no_or_nil);
        return;
    }

    if matches!(
        if_stmt.no_or_nil.data.as_deref(),
        Some(StmtData::Break(break_stmt)) if break_stmt.label.is_none()
    ) {
        for_stmt.test_or_nil = join_loop_test(for_stmt.test_or_nil.clone(), if_stmt.test);
        for_stmt.body = drop_first_statement(for_stmt.body.clone(), if_stmt.yes);
    }
}

fn join_loop_test(left: Expr, right: Expr) -> Expr {
    if left.data.is_none() {
        return right;
    }
    Expr::new(
        left.loc,
        ExprData::Binary(BinaryExpr {
            left,
            right,
            op: OpCode::BinaryLogicalAnd,
        }),
    )
}

pub(crate) fn append_if_or_label_body_preserving_scope(
    mut statements: Vec<Stmt>,
    body: Stmt,
) -> Vec<Stmt> {
    if let Some(StmtData::Block(block)) = body.data.as_deref()
        && !stmts_care_about_scope(&block.statements)
    {
        statements.extend(
            block
                .statements
                .iter()
                .filter(|statement| statement.data.is_some())
                .cloned(),
        );
    } else if stmt_cares_about_scope(&body) {
        statements.push(Stmt::new(
            body.loc,
            StmtData::Block(crate::internal::js_ast::BlockStmt {
                statements: vec![body],
                ..crate::internal::js_ast::BlockStmt::default()
            }),
        ));
    } else {
        statements.push(body);
    }
    statements
}

pub(crate) fn duplicate_case_hash(expr: &Expr) -> Option<u32> {
    match expr.data.as_deref()? {
        ExprData::InlinedEnum(value) => duplicate_case_hash(&value.value),
        ExprData::Null => Some(0),
        ExprData::Undefined => Some(1),
        ExprData::Boolean(value) => Some(hash_combine(2, u32::from(*value))),
        ExprData::Number(value) => {
            let bits = value.to_bits();
            let bytes = bits.to_le_bytes();
            Some(hash_combine(
                hash_combine(3, u32::from_le_bytes(bytes[..4].try_into().unwrap())),
                u32::from_le_bytes(bytes[4..].try_into().unwrap()),
            ))
        }
        ExprData::String(value) => Some(value.value.iter().fold(4, |hash, code_unit| {
            hash_combine(hash, u32::from(*code_unit))
        })),
        ExprData::BigInt(value) => Some(value.chars().fold(5, |hash, character| {
            hash_combine(hash, u32::from(character))
        })),
        ExprData::Identifier(value) => Some(hash_combine(6, value.reference.inner_index)),
        ExprData::Dot(value) => duplicate_case_hash(&value.target)
            .map(|target| hash_combine_string(hash_combine(7, target), &value.name)),
        ExprData::Index(value) => {
            let target = duplicate_case_hash(&value.target)?;
            let index = duplicate_case_hash(&value.index)?;
            Some(hash_combine(hash_combine(8, target), index))
        }
        _ => None,
    }
}

#[allow(clippy::float_cmp)]
pub(crate) fn duplicate_case_equals(left: &Expr, right: &Expr) -> (bool, bool) {
    if let Some(ExprData::InlinedEnum(value)) = right.data.as_deref() {
        return duplicate_case_equals(left, &value.value);
    }

    match (left.data.as_deref(), right.data.as_deref()) {
        (Some(ExprData::InlinedEnum(left)), _) => duplicate_case_equals(&left.value, right),
        (Some(ExprData::Null), Some(ExprData::Null))
        | (Some(ExprData::Undefined), Some(ExprData::Undefined)) => (true, false),
        (Some(ExprData::Boolean(left)), Some(ExprData::Boolean(right))) => (left == right, false),
        (Some(ExprData::Number(left)), Some(ExprData::Number(right))) => (left == right, false),
        (Some(ExprData::String(left)), Some(ExprData::String(right))) => {
            (left.value == right.value, false)
        }
        (Some(ExprData::BigInt(left)), Some(ExprData::BigInt(right))) => {
            (check_equality_big_int(left, right) == Some(true), false)
        }
        (Some(ExprData::Identifier(left)), Some(ExprData::Identifier(right))) => {
            (left.reference == right.reference, false)
        }
        (Some(ExprData::Dot(left)), Some(ExprData::Dot(right)))
            if left.optional_chain == right.optional_chain && left.name == right.name =>
        {
            (duplicate_case_equals(&left.target, &right.target).0, true)
        }
        (Some(ExprData::Index(left)), Some(ExprData::Index(right)))
            if left.optional_chain == right.optional_chain
                && duplicate_case_equals(&left.index, &right.index).0 =>
        {
            (duplicate_case_equals(&left.target, &right.target).0, true)
        }
        _ => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use crate::internal::{
        ast::Ref,
        js_ast::{
            BlockStmt, BreakStmt, ContinueStmt, DotExpr, Expr, ExprData, IdentifierExpr, IfStmt,
            InlinedEnumExpr, LocalKind, LocalStmt, OpCode, ReturnStmt, Stmt, StmtData, StringExpr,
            SwitchCase, SwitchStmt, UnaryExpr,
        },
        logger::Loc,
    };

    use super::{
        LivenessStatus, analyze_switch_cases_for_liveness, case_body_could_have_fall_through,
        duplicate_case_equals, duplicate_case_hash, is_jump_statement, jump_stmts_look_the_same,
        mangle_for,
    };

    fn expr(data: ExprData) -> Expr {
        Expr::new(Loc::default(), data)
    }

    fn stmt(data: StmtData) -> Stmt {
        Stmt::new(Loc::default(), data)
    }

    #[test]
    fn switch_case_liveness_tracks_default_and_fall_through() {
        let switch = SwitchStmt {
            test: expr(ExprData::Number(1.0)),
            cases: vec![
                SwitchCase {
                    value_or_nil: expr(ExprData::Number(0.0)),
                    body: vec![stmt(StmtData::Break(BreakStmt::default()))],
                    ..SwitchCase::default()
                },
                SwitchCase {
                    value_or_nil: expr(ExprData::Number(1.0)),
                    body: Vec::new(),
                    ..SwitchCase::default()
                },
                SwitchCase {
                    value_or_nil: Expr::default(),
                    body: vec![stmt(StmtData::Break(BreakStmt::default()))],
                    ..SwitchCase::default()
                },
            ],
            ..SwitchStmt::default()
        };
        let result = analyze_switch_cases_for_liveness(&switch);
        assert_eq!(result[0].status, LivenessStatus::AlwaysDead);
        assert_eq!(result[1].status, LivenessStatus::AlwaysLive);
        assert_eq!(result[2].status, LivenessStatus::Unknown);
    }

    #[test]
    fn dead_lexical_case_still_affects_shared_switch_scope() {
        let switch = SwitchStmt {
            test: expr(ExprData::Number(1.0)),
            cases: vec![
                SwitchCase {
                    value_or_nil: expr(ExprData::Number(0.0)),
                    body: vec![stmt(StmtData::Local(LocalStmt {
                        kind: LocalKind::Let,
                        ..LocalStmt::default()
                    }))],
                    ..SwitchCase::default()
                },
                SwitchCase {
                    value_or_nil: expr(ExprData::Number(1.0)),
                    ..SwitchCase::default()
                },
            ],
            ..SwitchStmt::default()
        };
        let result = analyze_switch_cases_for_liveness(&switch);
        assert_eq!(result[0].status, LivenessStatus::Unknown);
        assert_eq!(result[1].status, LivenessStatus::AlwaysLive);
    }

    #[test]
    fn nested_terminal_block_does_not_fall_through() {
        let body = vec![stmt(StmtData::Block(BlockStmt {
            statements: vec![stmt(StmtData::Break(BreakStmt::default()))],
            ..BlockStmt::default()
        }))];
        assert!(!case_body_could_have_fall_through(&body));
    }

    #[test]
    fn duplicate_case_hash_and_equality_cover_property_chains() {
        let reference = Ref {
            source_index: 1,
            inner_index: 2,
        };
        let property = |name: &str| {
            expr(ExprData::Dot(DotExpr {
                target: expr(ExprData::Identifier(IdentifierExpr {
                    reference,
                    ..IdentifierExpr::default()
                })),
                name: name.into(),
                ..DotExpr::default()
            }))
        };
        let left = property("x");
        let right = property("x");
        assert_eq!(duplicate_case_hash(&left), duplicate_case_hash(&right));
        assert_eq!(duplicate_case_equals(&left, &right), (true, true));
        assert_eq!(duplicate_case_equals(&left, &property("y")), (false, false));
    }

    #[test]
    fn inlined_enums_compare_as_their_values() {
        let value = expr(ExprData::String(StringExpr {
            value: "value".encode_utf16().collect(),
            ..StringExpr::default()
        }));
        let inlined = expr(ExprData::InlinedEnum(InlinedEnumExpr {
            value: value.clone(),
            ..InlinedEnumExpr::default()
        }));
        assert_eq!(duplicate_case_hash(&inlined), duplicate_case_hash(&value));
        assert_eq!(duplicate_case_equals(&inlined, &value), (true, false));
    }

    #[test]
    fn jump_statement_comparison_uses_labels_and_return_values() {
        let unlabeled_break = StmtData::Break(BreakStmt::default());
        let unlabeled_continue = StmtData::Continue(ContinueStmt::default());
        assert!(is_jump_statement(&unlabeled_break));
        assert!(!jump_stmts_look_the_same(
            &unlabeled_break,
            &unlabeled_continue
        ));

        let one = StmtData::Return(ReturnStmt {
            value_or_nil: expr(ExprData::Number(1.0)),
        });
        let another_one = StmtData::Return(ReturnStmt {
            value_or_nil: expr(ExprData::Number(1.0)),
        });
        let empty = StmtData::Return(ReturnStmt::default());
        assert!(jump_stmts_look_the_same(&one, &another_one));
        assert!(!jump_stmts_look_the_same(&one, &empty));
    }

    #[test]
    fn mangles_leading_break_condition_into_for_test() {
        let mut for_stmt = crate::internal::js_ast::ForStmt {
            test_or_nil: expr(ExprData::Identifier(IdentifierExpr {
                reference: Ref {
                    source_index: 1,
                    inner_index: 1,
                },
                ..IdentifierExpr::default()
            })),
            body: stmt(StmtData::If(IfStmt {
                test: expr(ExprData::Unary(UnaryExpr {
                    value: expr(ExprData::Boolean(true)),
                    op: OpCode::UnaryNot,
                    ..UnaryExpr::default()
                })),
                yes: stmt(StmtData::Break(BreakStmt::default())),
                ..IfStmt::default()
            })),
            ..crate::internal::js_ast::ForStmt::default()
        };
        mangle_for(&mut for_stmt);
        let Some(ExprData::Binary(test)) = for_stmt.test_or_nil.data.as_deref() else {
            panic!("expected combined loop test");
        };
        assert_eq!(test.op, OpCode::BinaryLogicalAnd);
        assert!(matches!(
            test.right.data.as_deref(),
            Some(ExprData::Boolean(true))
        ));
        assert!(matches!(
            for_stmt.body.data.as_deref(),
            Some(StmtData::Empty)
        ));
    }

    #[test]
    fn mangles_else_break_into_loop_test_and_yes_body() {
        let yes = stmt(StmtData::Debugger);
        let mut for_stmt = crate::internal::js_ast::ForStmt {
            body: stmt(StmtData::If(IfStmt {
                test: expr(ExprData::Boolean(true)),
                yes: yes.clone(),
                no_or_nil: stmt(StmtData::Break(BreakStmt::default())),
                ..IfStmt::default()
            })),
            ..crate::internal::js_ast::ForStmt::default()
        };
        mangle_for(&mut for_stmt);
        assert!(matches!(
            for_stmt.test_or_nil.data.as_deref(),
            Some(ExprData::Boolean(true))
        ));
        assert!(matches!(
            for_stmt.body.data.as_deref(),
            Some(StmtData::Debugger)
        ));
    }
}
