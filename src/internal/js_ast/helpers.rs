use super::{
    AnnotationFlags, ArrayExpr, AssignTarget, BinaryExpr, Binding, BindingData, Expr, ExprData,
    ExprStmt, IdentifierExpr, ObjectExpr, OpCode, OptionalChain, Property, PropertyFlags,
    PropertyKind, SpreadExpr, Stmt, StmtData, UnaryExpr,
};
use crate::internal::ast::Ref;
use crate::internal::compat::JsFeature;
use crate::internal::helpers::utf16_equals_wtf8;
use crate::internal::logger::Loc;
use std::ops::BitOr;

pub struct HelperContext<F> {
    is_unbound: F,
}

#[must_use]
pub fn make_helper_context<F>(is_unbound: F) -> HelperContext<F>
where
    F: Fn(Ref) -> bool,
{
    HelperContext { is_unbound }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StmtsCanBeRemovedIfUnusedFlags(u8);

impl StmtsCanBeRemovedIfUnusedFlags {
    pub const NONE: Self = Self(0);
    pub const KEEP_EXPORT_CLAUSES: Self = Self(1 << 0);
    pub const RETURN_CAN_BE_REMOVED_IF_UNUSED: Self = Self(1 << 1);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for StmtsCanBeRemovedIfUnusedFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl<F> HelperContext<F>
where
    F: Fn(Ref) -> bool,
{
    #[must_use]
    pub fn simplify_boolean_expr(&self, expr: &Expr) -> Expr {
        simplify_boolean_expr(expr, &|value| self.expr_can_be_removed_if_unused(value))
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn simplify_unused_expr(&self, expr: &Expr, unsupported_features: JsFeature) -> Expr {
        match expr.data.as_deref() {
            Some(ExprData::Annotation(value))
                if value
                    .flags
                    .contains(AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED) =>
            {
                return Expr::default();
            }
            Some(ExprData::InlinedEnum(value)) => {
                return self.simplify_unused_expr(&value.value, unsupported_features);
            }
            Some(
                ExprData::Null
                | ExprData::Undefined
                | ExprData::Missing
                | ExprData::Boolean(_)
                | ExprData::Number(_)
                | ExprData::BigInt(_)
                | ExprData::String(_)
                | ExprData::This
                | ExprData::RegExp(_)
                | ExprData::Function(_)
                | ExprData::Arrow(_)
                | ExprData::ImportMeta(_),
            ) => return Expr::default(),
            Some(ExprData::Dot(value)) if value.can_be_removed_if_unused => {
                return Expr::default();
            }
            Some(ExprData::Identifier(value))
                if !value.must_keep_due_to_with_stmt
                    && (value.can_be_removed_if_unused || !(self.is_unbound)(value.reference)) =>
            {
                return Expr::default();
            }
            Some(ExprData::Template(value)) if value.tag_or_nil.data.is_none() => {
                let mut comma = Expr::default();
                let mut pending_template: Option<(Loc, super::TemplateExpr)> = None;
                for part in &value.parts {
                    if known_primitive_type(part.value.data.as_deref()) == PrimitiveType::Unknown {
                        let (_, template) = pending_template.get_or_insert_with(|| {
                            (part.value.loc, super::TemplateExpr::default())
                        });
                        template.parts.push(super::TemplatePart {
                            value: part.value.clone(),
                            ..super::TemplatePart::default()
                        });
                    } else {
                        if let Some((loc, template)) = pending_template.take() {
                            comma = join_with_comma(
                                comma,
                                Expr::new(loc, ExprData::Template(template)),
                            );
                        }
                        comma = join_with_comma(
                            comma,
                            self.simplify_unused_expr(&part.value, unsupported_features),
                        );
                    }
                }
                if let Some((loc, template)) = pending_template {
                    comma = join_with_comma(comma, Expr::new(loc, ExprData::Template(template)));
                }
                return comma;
            }
            Some(ExprData::Template(value)) if value.can_be_unwrapped_if_unused => {
                return join_all_with_comma(
                    value
                        .parts
                        .iter()
                        .map(|part| self.simplify_unused_expr(&part.value, unsupported_features)),
                );
            }
            Some(ExprData::Array(value)) => {
                if value
                    .items
                    .iter()
                    .any(|item| matches!(item.data.as_deref(), Some(ExprData::Spread(_))))
                {
                    let items = value
                        .items
                        .iter()
                        .map(|item| self.simplify_unused_expr(item, unsupported_features))
                        .filter(|item| item.data.is_some())
                        .collect();
                    return Expr::new(
                        expr.loc,
                        ExprData::Array(ArrayExpr {
                            items,
                            ..value.clone()
                        }),
                    );
                }
                return join_all_with_comma(
                    value
                        .items
                        .iter()
                        .map(|item| self.simplify_unused_expr(item, unsupported_features)),
                );
            }
            Some(ExprData::Object(value)) => {
                if value
                    .properties
                    .iter()
                    .any(|property| property.kind == PropertyKind::Spread)
                {
                    let mut properties = Vec::with_capacity(value.properties.len());
                    for original in &value.properties {
                        let mut property = original.clone();
                        if property.kind != PropertyKind::Spread {
                            let simplified = self
                                .simplify_unused_expr(&property.value_or_nil, unsupported_features);
                            if simplified.data.is_some() {
                                property.value_or_nil = simplified;
                            } else if !property.flags.contains(PropertyFlags::IS_COMPUTED) {
                                continue;
                            } else {
                                property.value_or_nil =
                                    Expr::new(property.value_or_nil.loc, ExprData::Number(0.0));
                            }
                        }
                        properties.push(property);
                    }
                    return Expr::new(
                        expr.loc,
                        ExprData::Object(ObjectExpr {
                            properties,
                            ..value.clone()
                        }),
                    );
                }

                let mut result = Expr::default();
                for property in &value.properties {
                    if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                        result = join_with_comma(
                            result,
                            Expr::new(
                                property.key.loc,
                                ExprData::Binary(BinaryExpr {
                                    left: property.key.clone(),
                                    right: Expr::new(
                                        property.key.loc,
                                        ExprData::String(super::StringExpr::default()),
                                    ),
                                    op: OpCode::BinaryAdd,
                                }),
                            ),
                        );
                    }
                    result = join_with_comma(
                        result,
                        self.simplify_unused_expr(&property.value_or_nil, unsupported_features),
                    );
                }
                return result;
            }
            Some(ExprData::If(value)) => {
                let yes = self.simplify_unused_expr(&value.yes, unsupported_features);
                let no = self.simplify_unused_expr(&value.no, unsupported_features);
                if yes.data.is_none() && no.data.is_none() {
                    return self.simplify_unused_expr(&value.test, unsupported_features);
                }
                if yes.data.is_none() {
                    return join_with_left_associative_op(
                        OpCode::BinaryLogicalOr,
                        value.test.clone(),
                        no,
                    );
                }
                if no.data.is_none() {
                    return join_with_left_associative_op(
                        OpCode::BinaryLogicalAnd,
                        value.test.clone(),
                        yes,
                    );
                }
                return Expr::new(
                    expr.loc,
                    ExprData::If(super::IfExpr {
                        test: value.test.clone(),
                        yes,
                        no,
                    }),
                );
            }
            Some(ExprData::Unary(value)) => match value.op {
                OpCode::UnaryNegative
                    if matches!(value.value.data.as_deref(), Some(ExprData::BigInt(_))) =>
                {
                    return Expr::default();
                }
                OpCode::UnaryTypeof
                    if value.was_originally_typeof_identifier
                        && matches!(value.value.data.as_deref(), Some(ExprData::Identifier(_))) =>
                {
                    return Expr::default();
                }
                OpCode::UnaryVoid | OpCode::UnaryNot | OpCode::UnaryTypeof => {
                    return self.simplify_unused_expr(&value.value, unsupported_features);
                }
                _ => {}
            },
            Some(ExprData::Binary(value)) => {
                let mut left = value.left.clone();
                let mut right = value.right.clone();
                match value.op {
                    OpCode::BinaryStrictEqual
                    | OpCode::BinaryStrictNotEqual
                    | OpCode::BinaryComma => {
                        return join_with_comma(
                            self.simplify_unused_expr(&left, unsupported_features),
                            self.simplify_unused_expr(&right, unsupported_features),
                        );
                    }
                    OpCode::BinaryLooseEqual | OpCode::BinaryLooseNotEqual
                        if merged_known_primitive_types(&left, &right)
                            != PrimitiveType::Unknown =>
                    {
                        return join_with_comma(
                            self.simplify_unused_expr(&left, unsupported_features),
                            self.simplify_unused_expr(&right, unsupported_features),
                        );
                    }
                    OpCode::BinaryLogicalAnd
                    | OpCode::BinaryLogicalOr
                    | OpCode::BinaryNullishCoalescing => {
                        if value.op != OpCode::BinaryNullishCoalescing {
                            left = self.simplify_boolean_expr(&left);
                        }
                        right = self.simplify_unused_expr(&right, unsupported_features);
                        if right.data.is_none() {
                            return self.simplify_unused_expr(&left, unsupported_features);
                        }

                        if !unsupported_features.contains(JsFeature::OPTIONAL_CHAIN)
                            && let Some(ExprData::Binary(comparison)) = left.data.as_deref()
                            && ((comparison.op == OpCode::BinaryLooseNotEqual
                                && value.op == OpCode::BinaryLogicalAnd)
                                || (comparison.op == OpCode::BinaryLooseEqual
                                    && value.op == OpCode::BinaryLogicalOr))
                        {
                            let test =
                                if matches!(comparison.right.data.as_deref(), Some(ExprData::Null))
                                {
                                    Some(&comparison.left)
                                } else if matches!(
                                    comparison.left.data.as_deref(),
                                    Some(ExprData::Null)
                                ) {
                                    Some(&comparison.right)
                                } else {
                                    None
                                };
                            if let Some(test) = test
                                && matches!(
                                    test.data.as_deref(),
                                    Some(ExprData::Identifier(identifier))
                                        if !identifier.must_keep_due_to_with_stmt
                                )
                                && try_to_insert_optional_chain(test, &mut right)
                            {
                                return right;
                            }
                        }
                    }
                    OpCode::BinaryAdd => {
                        let (result, is_string_addition) =
                            simplify_unused_string_addition_chain(expr);
                        if is_string_addition {
                            return result;
                        }
                    }
                    _ => {}
                }
                return Expr::new(
                    expr.loc,
                    ExprData::Binary(BinaryExpr {
                        left,
                        right,
                        ..value.clone()
                    }),
                );
            }
            Some(ExprData::Call(value)) if value.can_be_unwrapped_if_unused => {
                return join_all_with_comma(value.args.iter().map(|argument| {
                    let argument = if matches!(argument.data.as_deref(), Some(ExprData::Spread(_)))
                    {
                        Expr::new(
                            argument.loc,
                            ExprData::Array(ArrayExpr {
                                items: vec![argument.clone()],
                                is_single_line: true,
                                ..ArrayExpr::default()
                            }),
                        )
                    } else {
                        argument.clone()
                    };
                    self.simplify_unused_expr(&argument, unsupported_features)
                }));
            }
            Some(ExprData::Call(value)) if value.args.is_empty() => {
                match value.target.data.as_deref() {
                    Some(ExprData::Function(target))
                        if target.function.args.is_empty()
                            && target.function.body.block.statements.is_empty() =>
                    {
                        return Expr::default();
                    }
                    Some(ExprData::Arrow(target)) if target.args.is_empty() => {
                        if target.body.block.statements.is_empty() {
                            return Expr::default();
                        }
                        if let [statement] = target.body.block.statements.as_slice() {
                            match statement.data.as_deref() {
                                Some(StmtData::Expr(statement)) if !target.is_async => {
                                    return statement.value.clone();
                                }
                                Some(StmtData::Expr(statement)) => {
                                    let mut target = target.clone();
                                    target.body.block.statements[0] = Stmt::new(
                                        statement.value.loc,
                                        StmtData::Return(super::ReturnStmt {
                                            value_or_nil: statement.value.clone(),
                                        }),
                                    );
                                    target.prefer_expr = true;
                                    return Expr::new(
                                        expr.loc,
                                        ExprData::Call(super::CallExpr {
                                            target: Expr::new(
                                                value.target.loc,
                                                ExprData::Arrow(target),
                                            ),
                                            ..super::CallExpr::default()
                                        }),
                                    );
                                }
                                Some(StmtData::Return(statement)) if !target.is_async => {
                                    return statement.value_or_nil.clone();
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some(ExprData::New(value)) if value.can_be_unwrapped_if_unused => {
                return join_all_with_comma(value.args.iter().map(|argument| {
                    let argument = if matches!(argument.data.as_deref(), Some(ExprData::Spread(_)))
                    {
                        Expr::new(
                            argument.loc,
                            ExprData::Array(ArrayExpr {
                                items: vec![argument.clone()],
                                is_single_line: true,
                                ..ArrayExpr::default()
                            }),
                        )
                    } else {
                        argument.clone()
                    };
                    self.simplify_unused_expr(&argument, unsupported_features)
                }));
            }
            _ => {}
        }
        expr.clone()
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    /// # Panics
    ///
    /// Panics if a default export contains an invalid internal statement kind.
    pub fn stmts_can_be_removed_if_unused(
        &self,
        statements: &[Stmt],
        flags: StmtsCanBeRemovedIfUnusedFlags,
    ) -> bool {
        for statement in statements {
            match statement.data.as_deref() {
                Some(
                    StmtData::Function(_)
                    | StmtData::Empty
                    | StmtData::Import(_)
                    | StmtData::ExportFrom(_),
                ) => {}
                Some(StmtData::Class(value)) => {
                    if !self.class_can_be_removed_if_unused(&value.class) {
                        return false;
                    }
                }
                Some(StmtData::Return(value)) => {
                    if !flags
                        .contains(StmtsCanBeRemovedIfUnusedFlags::RETURN_CAN_BE_REMOVED_IF_UNUSED)
                        || (value.value_or_nil.data.is_some()
                            && !self.expr_can_be_removed_if_unused(&value.value_or_nil))
                    {
                        return false;
                    }
                }
                Some(StmtData::Expr(value)) => {
                    if !self.expr_can_be_removed_if_unused(&value.value)
                        && !value.is_from_class_or_fn_that_can_be_removed_if_unused
                    {
                        return false;
                    }
                }
                Some(StmtData::Local(value)) => {
                    if value.kind == super::LocalKind::AwaitUsing {
                        return false;
                    }
                    for declaration in &value.declarations {
                        match declaration.binding.data.as_deref() {
                            Some(BindingData::Identifier(_)) => {}
                            Some(BindingData::Array(array))
                                if matches!(
                                    declaration.value_or_nil.data.as_deref(),
                                    Some(ExprData::Array(_))
                                ) =>
                            {
                                for item in &array.items {
                                    if item.default_value_or_nil.data.is_some()
                                        && !self.expr_can_be_removed_if_unused(
                                            &item.default_value_or_nil,
                                        )
                                    {
                                        return false;
                                    }
                                    if !matches!(
                                        item.binding.data.as_deref(),
                                        Some(BindingData::Identifier(_) | BindingData::Missing)
                                    ) {
                                        return false;
                                    }
                                }
                            }
                            _ => return false,
                        }
                        if declaration.value_or_nil.data.is_some() {
                            if !self.expr_can_be_removed_if_unused(&declaration.value_or_nil) {
                                return false;
                            }
                            if value.kind.is_using()
                                && !matches!(
                                    known_primitive_type(declaration.value_or_nil.data.as_deref()),
                                    PrimitiveType::Null | PrimitiveType::Undefined
                                )
                            {
                                return false;
                            }
                        }
                    }
                }
                Some(StmtData::Try(value)) => {
                    if !self.stmts_can_be_removed_if_unused(
                        &value.block.statements,
                        StmtsCanBeRemovedIfUnusedFlags::NONE,
                    ) || value.finally.as_ref().is_some_and(|finally| {
                        !self.stmts_can_be_removed_if_unused(
                            &finally.block.statements,
                            StmtsCanBeRemovedIfUnusedFlags::NONE,
                        )
                    }) {
                        return false;
                    }
                }
                Some(StmtData::ExportClause(_)) => {
                    if flags.contains(StmtsCanBeRemovedIfUnusedFlags::KEEP_EXPORT_CLAUSES) {
                        return false;
                    }
                }
                Some(StmtData::ExportDefault(value)) => match value.value.data.as_deref() {
                    Some(StmtData::Expr(expression)) => {
                        if !self.expr_can_be_removed_if_unused(&expression.value) {
                            return false;
                        }
                    }
                    Some(StmtData::Function(_)) => {}
                    Some(StmtData::Class(class)) => {
                        if !self.class_can_be_removed_if_unused(&class.class) {
                            return false;
                        }
                    }
                    _ => panic!("internal error: invalid default export statement"),
                },
                _ => return false,
            }
        }
        true
    }

    #[must_use]
    pub fn class_can_be_removed_if_unused(&self, class: &super::Class) -> bool {
        if !class.decorators.is_empty() {
            return false;
        }
        if class.extends_or_nil.data.is_some()
            && !self.expr_can_be_removed_if_unused(&class.extends_or_nil)
        {
            return false;
        }

        for property in &class.properties {
            if property.kind == PropertyKind::ClassStaticBlock {
                let Some(block) = &property.class_static_block else {
                    return false;
                };
                if !self.stmts_can_be_removed_if_unused(
                    &block.block.statements,
                    StmtsCanBeRemovedIfUnusedFlags::NONE,
                ) {
                    return false;
                }
                continue;
            }
            if !property.decorators.is_empty() {
                return false;
            }
            if property.flags.contains(PropertyFlags::IS_COMPUTED)
                && !is_primitive_literal(property.key.data.as_deref())
                && !is_symbol_instance(property.key.data.as_deref())
            {
                return false;
            }
            if property.kind.is_method_definition()
                && let Some(ExprData::Function(function)) = property.value_or_nil.data.as_deref()
                && function
                    .function
                    .args
                    .iter()
                    .any(|argument| !argument.decorators.is_empty())
            {
                return false;
            }
            if property.flags.contains(PropertyFlags::IS_STATIC) {
                if property.value_or_nil.data.is_some()
                    && !self.expr_can_be_removed_if_unused(&property.value_or_nil)
                {
                    return false;
                }
                if property.initializer_or_nil.data.is_some()
                    && !self.expr_can_be_removed_if_unused(&property.initializer_or_nil)
                {
                    return false;
                }
                if property.kind == PropertyKind::Field && !class.use_define_for_class_fields {
                    return false;
                }
            }
        }
        true
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn expr_can_be_removed_if_unused(&self, expr: &Expr) -> bool {
        match expr.data.as_deref() {
            Some(ExprData::Annotation(value)) => value
                .flags
                .contains(AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED),
            Some(ExprData::InlinedEnum(value)) => self.expr_can_be_removed_if_unused(&value.value),
            Some(
                ExprData::Null
                | ExprData::Undefined
                | ExprData::Missing
                | ExprData::Boolean(_)
                | ExprData::Number(_)
                | ExprData::BigInt(_)
                | ExprData::String(_)
                | ExprData::This
                | ExprData::RegExp(_)
                | ExprData::Function(_)
                | ExprData::Arrow(_)
                | ExprData::ImportMeta(_)
                | ExprData::ImportIdentifier(_),
            ) => true,
            Some(ExprData::Dot(value)) => value.can_be_removed_if_unused,
            Some(ExprData::Class(value)) => self.class_can_be_removed_if_unused(&value.class),
            Some(ExprData::Identifier(value)) => {
                !value.must_keep_due_to_with_stmt
                    && (value.can_be_removed_if_unused || !(self.is_unbound)(value.reference))
            }
            Some(ExprData::If(value)) => {
                self.expr_can_be_removed_if_unused(&value.test)
                    && (self.is_side_effect_free_unbound_identifier_ref(
                        &value.yes,
                        &value.test,
                        true,
                    ) || self.expr_can_be_removed_if_unused(&value.yes))
                    && (self.is_side_effect_free_unbound_identifier_ref(
                        &value.no,
                        &value.test,
                        false,
                    ) || self.expr_can_be_removed_if_unused(&value.no))
            }
            Some(ExprData::Array(value)) => value.items.iter().all(|item| {
                let item = match item.data.as_deref() {
                    Some(ExprData::Spread(spread))
                        if matches!(spread.value.data.as_deref(), Some(ExprData::Array(_))) =>
                    {
                        &spread.value
                    }
                    _ => item,
                };
                self.expr_can_be_removed_if_unused(item)
            }),
            Some(ExprData::Object(value)) => value.properties.iter().all(|property| {
                property.kind != PropertyKind::Spread
                    && (!property.flags.contains(PropertyFlags::IS_COMPUTED)
                        || is_primitive_literal(property.key.data.as_deref())
                        || is_symbol_instance(property.key.data.as_deref()))
                    && (property.value_or_nil.data.is_none()
                        || self.expr_can_be_removed_if_unused(&property.value_or_nil))
            }),
            Some(ExprData::Call(value)) if value.can_be_unwrapped_if_unused => value
                .args
                .iter()
                .all(|argument| self.expr_can_be_removed_if_unused(argument)),
            Some(ExprData::New(value)) if value.can_be_unwrapped_if_unused => value
                .args
                .iter()
                .all(|argument| self.expr_can_be_removed_if_unused(argument)),
            Some(ExprData::Unary(value)) => match value.op {
                OpCode::UnaryVoid | OpCode::UnaryNot => {
                    self.expr_can_be_removed_if_unused(&value.value)
                }
                OpCode::UnaryNegative
                    if matches!(value.value.data.as_deref(), Some(ExprData::BigInt(_))) =>
                {
                    true
                }
                OpCode::UnaryTypeof
                    if value.was_originally_typeof_identifier
                        && matches!(value.value.data.as_deref(), Some(ExprData::Identifier(_))) =>
                {
                    true
                }
                OpCode::UnaryTypeof => self.expr_can_be_removed_if_unused(&value.value),
                _ => false,
            },
            Some(ExprData::Binary(value)) => match value.op {
                OpCode::BinaryStrictEqual
                | OpCode::BinaryStrictNotEqual
                | OpCode::BinaryComma
                | OpCode::BinaryNullishCoalescing => {
                    self.expr_can_be_removed_if_unused(&value.left)
                        && self.expr_can_be_removed_if_unused(&value.right)
                }
                OpCode::BinaryLogicalOr => {
                    self.expr_can_be_removed_if_unused(&value.left)
                        && (self.is_side_effect_free_unbound_identifier_ref(
                            &value.right,
                            &value.left,
                            false,
                        ) || self.expr_can_be_removed_if_unused(&value.right))
                }
                OpCode::BinaryLogicalAnd => {
                    self.expr_can_be_removed_if_unused(&value.left)
                        && (self.is_side_effect_free_unbound_identifier_ref(
                            &value.right,
                            &value.left,
                            true,
                        ) || self.expr_can_be_removed_if_unused(&value.right))
                }
                OpCode::BinaryLooseEqual | OpCode::BinaryLooseNotEqual => {
                    can_change_strict_to_loose(&value.left, &value.right)
                        && self.expr_can_be_removed_if_unused(&value.left)
                        && self.expr_can_be_removed_if_unused(&value.right)
                }
                OpCode::BinaryLessThan
                | OpCode::BinaryGreaterThan
                | OpCode::BinaryLessThanOrEqual
                | OpCode::BinaryGreaterThanOrEqual => {
                    let left = known_primitive_type(value.left.data.as_deref());
                    matches!(
                        left,
                        PrimitiveType::String | PrimitiveType::Number | PrimitiveType::BigInt
                    ) && known_primitive_type(value.right.data.as_deref()) == left
                        && self.expr_can_be_removed_if_unused(&value.left)
                        && self.expr_can_be_removed_if_unused(&value.right)
                }
                _ => false,
            },
            Some(ExprData::Template(value))
                if value.tag_or_nil.data.is_none() || value.can_be_unwrapped_if_unused =>
            {
                value.parts.iter().all(|part| {
                    self.expr_can_be_removed_if_unused(&part.value)
                        && known_primitive_type(part.value.data.as_deref())
                            != PrimitiveType::Unknown
                })
            }
            _ => false,
        }
    }

    fn is_side_effect_free_unbound_identifier_ref(
        &self,
        value: &Expr,
        guard_condition: &Expr,
        mut is_yes_branch: bool,
    ) -> bool {
        let Some(ExprData::Identifier(identifier)) = value.data.as_deref() else {
            return false;
        };
        if !(self.is_unbound)(identifier.reference) {
            return false;
        }
        let Some(ExprData::Binary(binary)) = guard_condition.data.as_deref() else {
            return false;
        };

        match binary.op {
            OpCode::BinaryStrictEqual
            | OpCode::BinaryStrictNotEqual
            | OpCode::BinaryLooseEqual
            | OpCode::BinaryLooseNotEqual => {
                let (mut typeof_expr, mut string_expr) = (&binary.left, &binary.right);
                if matches!(typeof_expr.data.as_deref(), Some(ExprData::String(_))) {
                    std::mem::swap(&mut typeof_expr, &mut string_expr);
                }
                if let (Some(ExprData::Unary(unary)), Some(ExprData::String(text))) =
                    (typeof_expr.data.as_deref(), string_expr.data.as_deref())
                    && unary.op == OpCode::UnaryTypeof
                    && unary.was_originally_typeof_identifier
                    && (utf16_equals_wtf8(&text.value, b"undefined") == is_yes_branch)
                        == matches!(
                            binary.op,
                            OpCode::BinaryStrictNotEqual | OpCode::BinaryLooseNotEqual
                        )
                    && matches!(
                        unary.value.data.as_deref(),
                        Some(ExprData::Identifier(guarded))
                            if guarded.reference == identifier.reference
                    )
                {
                    return true;
                }
            }
            OpCode::BinaryLessThan
            | OpCode::BinaryGreaterThan
            | OpCode::BinaryLessThanOrEqual
            | OpCode::BinaryGreaterThanOrEqual => {
                let (mut typeof_expr, mut string_expr) = (&binary.left, &binary.right);
                if matches!(typeof_expr.data.as_deref(), Some(ExprData::String(_))) {
                    std::mem::swap(&mut typeof_expr, &mut string_expr);
                    is_yes_branch = !is_yes_branch;
                }
                if let (Some(ExprData::Unary(unary)), Some(ExprData::String(text))) =
                    (typeof_expr.data.as_deref(), string_expr.data.as_deref())
                    && unary.op == OpCode::UnaryTypeof
                    && unary.was_originally_typeof_identifier
                    && utf16_equals_wtf8(&text.value, b"u")
                    && is_yes_branch
                        == matches!(
                            binary.op,
                            OpCode::BinaryLessThan | OpCode::BinaryLessThanOrEqual
                        )
                    && matches!(
                        unary.value.data.as_deref(),
                        Some(ExprData::Identifier(guarded))
                            if guarded.reference == identifier.reference
                    )
                {
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}

#[must_use]
pub fn is_property_access(expr: &Expr) -> bool {
    matches!(
        expr.data.as_deref(),
        Some(ExprData::Dot(_) | ExprData::Index(_))
    )
}

#[must_use]
pub fn is_optional_chain(expr: &Expr) -> bool {
    match expr.data.as_deref() {
        Some(ExprData::Dot(value)) => value.optional_chain != OptionalChain::None,
        Some(ExprData::Index(value)) => value.optional_chain != OptionalChain::None,
        Some(ExprData::Call(value)) => value.optional_chain != OptionalChain::None,
        _ => false,
    }
}

#[must_use]
pub fn assign(left: Expr, right: Expr) -> Expr {
    Expr::new(
        left.loc,
        ExprData::Binary(BinaryExpr {
            left,
            right,
            op: OpCode::BinaryAssign,
        }),
    )
}

#[must_use]
pub fn assign_stmt(left: Expr, right: Expr) -> Stmt {
    let loc = left.loc;
    Stmt::new(
        loc,
        StmtData::Expr(ExprStmt {
            value: assign(left, right),
            ..ExprStmt::default()
        }),
    )
}

#[must_use]
pub fn not(expr: Expr) -> Expr {
    if let Some(result) = maybe_simplify_not(&expr) {
        return result;
    }
    Expr::new(
        expr.loc,
        ExprData::Unary(UnaryExpr {
            value: expr,
            op: OpCode::UnaryNot,
            ..UnaryExpr::default()
        }),
    )
}

#[must_use]
pub fn maybe_simplify_not(expr: &Expr) -> Option<Expr> {
    let data = expr.data.as_deref()?;
    let boolean = |value| Some(Expr::new(expr.loc, ExprData::Boolean(value)));
    match data {
        ExprData::Annotation(annotation) => maybe_simplify_not(&annotation.value),
        ExprData::InlinedEnum(value) => maybe_simplify_not(&value.value),
        ExprData::Null | ExprData::Undefined => boolean(true),
        ExprData::Boolean(value) => boolean(!value),
        ExprData::Number(value) => boolean(*value == 0.0 || value.is_nan()),
        ExprData::String(value) => boolean(value.value.is_empty()),
        ExprData::BigInt(value) => check_equality_big_int(value, "0")
            .map(|equal| Expr::new(expr.loc, ExprData::Boolean(equal))),
        ExprData::Function(_) | ExprData::Arrow(_) | ExprData::RegExp(_) => boolean(false),
        ExprData::Unary(unary)
            if unary.op == OpCode::UnaryNot
                && known_primitive_type(unary.value.data.as_deref()) == PrimitiveType::Boolean =>
        {
            Some(unary.value.clone())
        }
        ExprData::Binary(binary) => {
            let flipped = match binary.op {
                OpCode::BinaryLooseEqual => Some(OpCode::BinaryLooseNotEqual),
                OpCode::BinaryLooseNotEqual => Some(OpCode::BinaryLooseEqual),
                OpCode::BinaryStrictEqual => Some(OpCode::BinaryStrictNotEqual),
                OpCode::BinaryStrictNotEqual => Some(OpCode::BinaryStrictEqual),
                _ => None,
            };
            if let Some(op) = flipped {
                return Some(Expr::new(
                    expr.loc,
                    ExprData::Binary(BinaryExpr {
                        left: binary.left.clone(),
                        right: binary.right.clone(),
                        op,
                    }),
                ));
            }
            if binary.op == OpCode::BinaryComma {
                return Some(Expr::new(
                    expr.loc,
                    ExprData::Binary(BinaryExpr {
                        left: binary.left.clone(),
                        right: not(binary.right.clone()),
                        op: OpCode::BinaryComma,
                    }),
                ));
            }
            None
        }
        _ => None,
    }
}

#[must_use]
pub fn maybe_simplify_equality_comparison(
    loc: Loc,
    binary: &BinaryExpr,
    unsupported_features: JsFeature,
) -> Option<Expr> {
    let mut value = &binary.left;
    let mut primitive = &binary.right;
    let primitive_first = is_primitive_literal(value.data.as_deref());
    if primitive_first {
        std::mem::swap(&mut value, &mut primitive);
    }

    if let Some(ExprData::Boolean(boolean)) = primitive.data.as_deref()
        && known_primitive_type(value.data.as_deref()) == PrimitiveType::Boolean
    {
        let is_not_equal = matches!(
            binary.op,
            OpCode::BinaryLooseNotEqual | OpCode::BinaryStrictNotEqual
        );
        return Some(if *boolean == is_not_equal {
            not(value.clone())
        } else {
            value.clone()
        });
    }

    if !unsupported_features.contains(JsFeature::TYPEOF_EXOTIC_OBJECT_IS_OBJECT)
        && matches!(
            value.data.as_deref(),
            Some(ExprData::Unary(unary)) if unary.op == OpCode::UnaryTypeof
        )
        && matches!(
            primitive.data.as_deref(),
            Some(ExprData::String(string)) if utf16_equals_wtf8(&string.value, b"undefined")
        )
    {
        let is_equal = matches!(
            binary.op,
            OpCode::BinaryLooseEqual | OpCode::BinaryStrictEqual
        );
        let op = if is_equal == primitive_first {
            OpCode::BinaryLessThan
        } else {
            OpCode::BinaryGreaterThan
        };
        let short_undefined = Expr::new(
            primitive.loc,
            ExprData::String(super::StringExpr {
                value: vec![u16::from(b'u')],
                ..super::StringExpr::default()
            }),
        );
        let (left, right) = if primitive_first {
            (short_undefined, value.clone())
        } else {
            (value.clone(), short_undefined)
        };
        return Some(Expr::new(
            loc,
            ExprData::Binary(BinaryExpr { left, right, op }),
        ));
    }

    None
}

#[must_use]
pub fn is_symbol_instance(data: Option<&ExprData>) -> bool {
    match data {
        Some(ExprData::Dot(value)) => value.is_symbol_instance,
        Some(ExprData::Index(value)) => value.is_symbol_instance,
        _ => false,
    }
}

#[must_use]
pub fn is_primitive_literal(data: Option<&ExprData>) -> bool {
    match data {
        Some(ExprData::Annotation(value)) => is_primitive_literal(value.value.data.as_deref()),
        Some(ExprData::InlinedEnum(value)) => is_primitive_literal(value.value.data.as_deref()),
        Some(
            ExprData::Null
            | ExprData::Undefined
            | ExprData::String(_)
            | ExprData::Boolean(_)
            | ExprData::Number(_)
            | ExprData::BigInt(_),
        ) => true,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum PrimitiveType {
    #[default]
    Unknown,
    Mixed,
    Null,
    Undefined,
    Boolean,
    Number,
    String,
    BigInt,
}

#[must_use]
pub fn merged_known_primitive_types(left: &Expr, right: &Expr) -> PrimitiveType {
    let left = known_primitive_type(left.data.as_deref());
    if left == PrimitiveType::Unknown {
        return PrimitiveType::Unknown;
    }
    let right = known_primitive_type(right.data.as_deref());
    if right == PrimitiveType::Unknown {
        return PrimitiveType::Unknown;
    }
    if left == right {
        left
    } else {
        PrimitiveType::Mixed
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn known_primitive_type(data: Option<&ExprData>) -> PrimitiveType {
    let Some(data) = data else {
        return PrimitiveType::Unknown;
    };
    match data {
        ExprData::Annotation(value) => known_primitive_type(value.value.data.as_deref()),
        ExprData::InlinedEnum(value) => known_primitive_type(value.value.data.as_deref()),
        ExprData::Null => PrimitiveType::Null,
        ExprData::Undefined => PrimitiveType::Undefined,
        ExprData::Boolean(_) => PrimitiveType::Boolean,
        ExprData::Number(_) => PrimitiveType::Number,
        ExprData::String(_) => PrimitiveType::String,
        ExprData::BigInt(_) => PrimitiveType::BigInt,
        ExprData::Template(value) if value.tag_or_nil.data.is_none() => PrimitiveType::String,
        ExprData::If(value) => merged_known_primitive_types(&value.yes, &value.no),
        ExprData::Unary(value) => match value.op {
            OpCode::UnaryVoid => PrimitiveType::Undefined,
            OpCode::UnaryTypeof => PrimitiveType::String,
            OpCode::UnaryNot | OpCode::UnaryDelete => PrimitiveType::Boolean,
            OpCode::UnaryPositive => PrimitiveType::Number,
            OpCode::UnaryNegative | OpCode::UnaryComplement => {
                match known_primitive_type(value.value.data.as_deref()) {
                    PrimitiveType::BigInt => PrimitiveType::BigInt,
                    PrimitiveType::Unknown | PrimitiveType::Mixed => PrimitiveType::Mixed,
                    _ => PrimitiveType::Number,
                }
            }
            OpCode::UnaryPreDecrement
            | OpCode::UnaryPreIncrement
            | OpCode::UnaryPostDecrement
            | OpCode::UnaryPostIncrement => PrimitiveType::Mixed,
            _ => PrimitiveType::Unknown,
        },
        ExprData::Binary(value) => match value.op {
            OpCode::BinaryStrictEqual
            | OpCode::BinaryStrictNotEqual
            | OpCode::BinaryLooseEqual
            | OpCode::BinaryLooseNotEqual
            | OpCode::BinaryLessThan
            | OpCode::BinaryGreaterThan
            | OpCode::BinaryLessThanOrEqual
            | OpCode::BinaryGreaterThanOrEqual
            | OpCode::BinaryInstanceof
            | OpCode::BinaryIn => PrimitiveType::Boolean,
            OpCode::BinaryLogicalOr | OpCode::BinaryLogicalAnd => {
                merged_known_primitive_types(&value.left, &value.right)
            }
            OpCode::BinaryNullishCoalescing => {
                let left = known_primitive_type(value.left.data.as_deref());
                let right = known_primitive_type(value.right.data.as_deref());
                match left {
                    PrimitiveType::Null | PrimitiveType::Undefined => right,
                    PrimitiveType::Unknown => PrimitiveType::Unknown,
                    PrimitiveType::Mixed => {
                        if right == PrimitiveType::Unknown {
                            PrimitiveType::Unknown
                        } else {
                            PrimitiveType::Mixed
                        }
                    }
                    _ => left,
                }
            }
            OpCode::BinaryAdd => {
                let left = known_primitive_type(value.left.data.as_deref());
                let right = known_primitive_type(value.right.data.as_deref());
                if left == PrimitiveType::String || right == PrimitiveType::String {
                    PrimitiveType::String
                } else if left == PrimitiveType::BigInt && right == PrimitiveType::BigInt {
                    PrimitiveType::BigInt
                } else if !matches!(
                    left,
                    PrimitiveType::Unknown | PrimitiveType::Mixed | PrimitiveType::BigInt
                ) && !matches!(
                    right,
                    PrimitiveType::Unknown | PrimitiveType::Mixed | PrimitiveType::BigInt
                ) {
                    PrimitiveType::Number
                } else {
                    PrimitiveType::Mixed
                }
            }
            OpCode::BinaryAddAssign => {
                if known_primitive_type(value.right.data.as_deref()) == PrimitiveType::String {
                    PrimitiveType::String
                } else {
                    PrimitiveType::Mixed
                }
            }
            OpCode::BinarySubtract
            | OpCode::BinarySubtractAssign
            | OpCode::BinaryMultiply
            | OpCode::BinaryMultiplyAssign
            | OpCode::BinaryDivide
            | OpCode::BinaryDivideAssign
            | OpCode::BinaryRemainder
            | OpCode::BinaryRemainderAssign
            | OpCode::BinaryPower
            | OpCode::BinaryPowerAssign
            | OpCode::BinaryBitwiseAnd
            | OpCode::BinaryBitwiseAndAssign
            | OpCode::BinaryBitwiseOr
            | OpCode::BinaryBitwiseOrAssign
            | OpCode::BinaryBitwiseXor
            | OpCode::BinaryBitwiseXorAssign
            | OpCode::BinaryShiftLeft
            | OpCode::BinaryShiftLeftAssign
            | OpCode::BinaryShiftRight
            | OpCode::BinaryShiftRightAssign
            | OpCode::BinaryUnsignedShiftRight
            | OpCode::BinaryUnsignedShiftRightAssign => PrimitiveType::Mixed,
            OpCode::BinaryAssign | OpCode::BinaryComma => {
                known_primitive_type(value.right.data.as_deref())
            }
            _ => PrimitiveType::Unknown,
        },
        _ => PrimitiveType::Unknown,
    }
}

#[must_use]
pub fn can_change_strict_to_loose(left: &Expr, right: &Expr) -> bool {
    let left = known_primitive_type(left.data.as_deref());
    let right = known_primitive_type(right.data.as_deref());
    left == right && !matches!(left, PrimitiveType::Unknown | PrimitiveType::Mixed)
}

#[must_use]
pub fn typeof_without_side_effects(data: Option<&ExprData>) -> Option<&'static str> {
    match data {
        Some(ExprData::Annotation(value))
            if value
                .flags
                .contains(AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED) =>
        {
            typeof_without_side_effects(value.value.data.as_deref())
        }
        Some(ExprData::InlinedEnum(value)) => {
            typeof_without_side_effects(value.value.data.as_deref())
        }
        Some(ExprData::Null) => Some("object"),
        Some(ExprData::Undefined) => Some("undefined"),
        Some(ExprData::Boolean(_)) => Some("boolean"),
        Some(ExprData::Number(_)) => Some("number"),
        Some(ExprData::BigInt(_)) => Some("bigint"),
        Some(ExprData::String(_)) => Some("string"),
        Some(ExprData::Function(_) | ExprData::Arrow(_)) => Some("function"),
        _ => None,
    }
}

#[must_use]
pub fn join_with_left_associative_op(op: OpCode, mut left: Expr, mut right: Expr) -> Expr {
    if let Some(ExprData::Binary(comma)) = left.data.as_deref()
        && comma.op == OpCode::BinaryComma
    {
        return Expr::new(
            left.loc,
            ExprData::Binary(BinaryExpr {
                left: comma.left.clone(),
                right: join_with_left_associative_op(op, comma.right.clone(), right),
                op: OpCode::BinaryComma,
            }),
        );
    }
    while let Some(ExprData::Binary(binary)) = right.data.as_deref() {
        if binary.op != op {
            break;
        }
        let binary_left = binary.left.clone();
        let binary_right = binary.right.clone();
        left = join_with_left_associative_op(op, left, binary_left);
        right = binary_right;
    }
    Expr::new(left.loc, ExprData::Binary(BinaryExpr { left, right, op }))
}

#[must_use]
pub fn join_with_comma(left: Expr, right: Expr) -> Expr {
    if left.data.is_none() {
        return right;
    }
    if right.data.is_none() {
        return left;
    }
    join_with_left_associative_op(OpCode::BinaryComma, left, right)
}

#[must_use]
pub fn join_all_with_comma(values: impl IntoIterator<Item = Expr>) -> Expr {
    values.into_iter().fold(Expr::default(), join_with_comma)
}

/// # Panics
///
/// Panics if `binding` has no data.
#[must_use]
pub fn convert_binding_to_expr(
    binding: &Binding,
    wrap_identifier: Option<&dyn Fn(Loc, Ref) -> Expr>,
) -> Expr {
    let data = binding
        .data
        .as_deref()
        .expect("internal error: missing binding data");
    match data {
        BindingData::Missing => Expr::new(binding.loc, ExprData::Missing),
        BindingData::Identifier(value) => wrap_identifier.map_or_else(
            || {
                Expr::new(
                    binding.loc,
                    ExprData::Identifier(IdentifierExpr {
                        reference: value.reference,
                        ..IdentifierExpr::default()
                    }),
                )
            },
            |wrap| wrap(binding.loc, value.reference),
        ),
        BindingData::Array(value) => {
            let last = value.items.len().saturating_sub(1);
            let items = value
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let expression = convert_binding_to_expr(&item.binding, wrap_identifier);
                    if value.has_spread && index == last {
                        Expr::new(
                            expression.loc,
                            ExprData::Spread(SpreadExpr { value: expression }),
                        )
                    } else if item.default_value_or_nil.data.is_some() {
                        assign(expression, item.default_value_or_nil.clone())
                    } else {
                        expression
                    }
                })
                .collect();
            Expr::new(
                binding.loc,
                ExprData::Array(ArrayExpr {
                    items,
                    is_single_line: value.is_single_line,
                    ..ArrayExpr::default()
                }),
            )
        }
        BindingData::Object(value) => {
            let properties = value
                .properties
                .iter()
                .map(|property| {
                    let mut flags = PropertyFlags::NONE;
                    if property.is_computed {
                        flags |= PropertyFlags::IS_COMPUTED;
                    }
                    Property {
                        kind: if property.is_spread {
                            PropertyKind::Spread
                        } else {
                            PropertyKind::Field
                        },
                        flags,
                        key: property.key.clone(),
                        value_or_nil: convert_binding_to_expr(&property.value, wrap_identifier),
                        initializer_or_nil: property.default_value_or_nil.clone(),
                        ..Property::default()
                    }
                })
                .collect();
            Expr::new(
                binding.loc,
                ExprData::Object(ObjectExpr {
                    properties,
                    is_single_line: value.is_single_line,
                    ..ObjectExpr::default()
                }),
            )
        }
    }
}

#[must_use]
pub const fn unary_assign_target_for(op: OpCode) -> AssignTarget {
    match op {
        OpCode::UnaryPreDecrement
        | OpCode::UnaryPreIncrement
        | OpCode::UnaryPostDecrement
        | OpCode::UnaryPostIncrement => AssignTarget::Update,
        _ => AssignTarget::None,
    }
}

#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp
)]
pub fn to_int32(value: f64) -> i32 {
    let easy = value as i32;
    if f64::from(easy) == value {
        return easy;
    }
    if !value.is_finite() {
        return 0;
    }
    let wrapped = ((value.abs() % 4_294_967_296.0) as u32).cast_signed();
    if value.is_sign_negative() {
        wrapped.wrapping_neg()
    } else {
        wrapped
    }
}

#[must_use]
#[allow(clippy::cast_sign_loss)]
pub fn to_uint32(value: f64) -> u32 {
    to_int32(value) as u32
}

#[must_use]
pub fn is_int32_or_uint32(data: Option<&ExprData>) -> bool {
    match data {
        Some(ExprData::Binary(value)) => match value.op {
            OpCode::BinaryUnsignedShiftRight => true,
            OpCode::BinaryLogicalOr | OpCode::BinaryLogicalAnd => {
                is_int32_or_uint32(value.left.data.as_deref())
                    && is_int32_or_uint32(value.right.data.as_deref())
            }
            _ => false,
        },
        Some(ExprData::If(value)) => {
            is_int32_or_uint32(value.yes.data.as_deref())
                && is_int32_or_uint32(value.no.data.as_deref())
        }
        _ => false,
    }
}

#[must_use]
pub fn to_number_without_side_effects(data: Option<&ExprData>) -> Option<f64> {
    match data? {
        ExprData::Annotation(value) => to_number_without_side_effects(value.value.data.as_deref()),
        ExprData::InlinedEnum(value) => to_number_without_side_effects(value.value.data.as_deref()),
        ExprData::Null => Some(0.0),
        ExprData::Undefined | ExprData::RegExp(_) => Some(f64::NAN),
        ExprData::Array(value) if value.items.is_empty() => Some(0.0),
        ExprData::Object(value) if value.properties.is_empty() => Some(f64::NAN),
        ExprData::Boolean(value) => Some(if *value { 1.0 } else { 0.0 }),
        ExprData::Number(value) => Some(*value),
        ExprData::String(value) if value.value.is_empty() => Some(0.0),
        ExprData::String(value) => string_to_equivalent_number_value(&value.value),
        _ => None,
    }
}

#[must_use]
pub fn to_string_without_side_effects(data: Option<&ExprData>) -> Option<String> {
    match data? {
        ExprData::Null => Some("null".into()),
        ExprData::Undefined => Some("undefined".into()),
        ExprData::Boolean(value) => Some(if *value { "true" } else { "false" }.into()),
        ExprData::BigInt(value) if value.len() < 2 || !value.starts_with('0') => {
            Some(value.clone())
        }
        ExprData::Number(value) => try_to_string_on_number_safely(*value, 10),
        ExprData::RegExp(value) => Some(value.clone()),
        ExprData::Dot(value) if value.name == "constructor" => match value.target.data.as_deref() {
            Some(ExprData::String(_)) => Some("function String() { [native code] }".into()),
            Some(ExprData::RegExp(_)) => Some("function RegExp() { [native code] }".into()),
            _ => None,
        },
        _ => None,
    }
}

fn extract_numeric_value(data: Option<&ExprData>) -> Option<f64> {
    match data? {
        ExprData::Annotation(value) => extract_numeric_value(value.value.data.as_deref()),
        ExprData::InlinedEnum(value) => extract_numeric_value(value.value.data.as_deref()),
        ExprData::Number(value) => Some(*value),
        _ => None,
    }
}

fn extract_numeric_values(left: &Expr, right: &Expr) -> Option<(f64, f64)> {
    Some((
        extract_numeric_value(left.data.as_deref())?,
        extract_numeric_value(right.data.as_deref())?,
    ))
}

fn extract_string_value(data: Option<&ExprData>) -> Option<&[u16]> {
    match data? {
        ExprData::Annotation(value) => extract_string_value(value.value.data.as_deref()),
        ExprData::InlinedEnum(value) => extract_string_value(value.value.data.as_deref()),
        ExprData::String(value) => Some(&value.value),
        _ => None,
    }
}

fn extract_string_values<'a>(left: &'a Expr, right: &'a Expr) -> Option<(&'a [u16], &'a [u16])> {
    Some((
        extract_string_value(left.data.as_deref())?,
        extract_string_value(right.data.as_deref())?,
    ))
}

#[must_use]
pub fn string_compare_ucs2(left: &[u16], right: &[u16]) -> i32 {
    for (&left, &right) in left.iter().zip(right) {
        let difference = i32::from(left) - i32::from(right);
        if difference != 0 {
            return difference;
        }
    }
    i32::try_from(left.len())
        .unwrap_or(i32::MAX)
        .saturating_sub(i32::try_from(right.len()).unwrap_or(i32::MAX))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn approximate_printed_int_char_count(value: f64) -> usize {
    let mut count = 1 + value.abs().log10().floor().max(0.0) as usize;
    if value.is_sign_negative() {
        count += 1;
    }
    count
}

#[must_use]
#[allow(clippy::float_cmp)]
pub fn should_fold_binary_operator_when_minifying(binary: &BinaryExpr) -> bool {
    match binary.op {
        OpCode::BinaryLooseEqual
        | OpCode::BinaryLooseNotEqual
        | OpCode::BinaryStrictEqual
        | OpCode::BinaryStrictNotEqual
        | OpCode::BinaryShiftRight
        | OpCode::BinaryBitwiseAnd
        | OpCode::BinaryBitwiseOr
        | OpCode::BinaryBitwiseXor
        | OpCode::BinaryLessThan
        | OpCode::BinaryGreaterThan
        | OpCode::BinaryLessThanOrEqual
        | OpCode::BinaryGreaterThanOrEqual => true,
        OpCode::BinaryAdd => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right)
                && left == left.trunc()
                && left.abs() <= 0xffff_ffff_u32.into()
                && right == right.trunc()
                && right.abs() <= 0xffff_ffff_u32.into()
            {
                return true;
            }
            extract_string_values(&binary.left, &binary.right).is_some()
        }
        OpCode::BinarySubtract => {
            extract_numeric_values(&binary.left, &binary.right).is_some_and(|(left, right)| {
                left == left.trunc()
                    && left.abs() <= f64::from(0xffff_ffff_u32)
                    && right == right.trunc()
                    && right.abs() <= f64::from(0xffff_ffff_u32)
            })
        }
        OpCode::BinaryMultiply => {
            extract_numeric_values(&binary.left, &binary.right).is_some_and(|(left, right)| {
                left == left.trunc()
                    && left.abs() <= 255.0
                    && right == right.trunc()
                    && right.abs() <= 255.0
            })
        }
        OpCode::BinaryDivide => extract_numeric_values(&binary.left, &binary.right)
            .is_some_and(|(_, right)| right == 0.0),
        OpCode::BinaryShiftLeft => {
            extract_numeric_values(&binary.left, &binary.right).is_some_and(|(left, right)| {
                let left_len = approximate_printed_int_char_count(left);
                let right_len = approximate_printed_int_char_count(right);
                let result = to_int32(left) << (to_uint32(right) & 31);
                approximate_printed_int_char_count(f64::from(result)) <= left_len + 2 + right_len
            })
        }
        OpCode::BinaryUnsignedShiftRight => extract_numeric_values(&binary.left, &binary.right)
            .is_some_and(|(left, right)| {
                let left_len = approximate_printed_int_char_count(left);
                let right_len = approximate_printed_int_char_count(right);
                let result = to_uint32(left) >> (to_uint32(right) & 31);
                approximate_printed_int_char_count(f64::from(result)) <= left_len + 3 + right_len
            }),
        OpCode::BinaryLogicalAnd | OpCode::BinaryLogicalOr | OpCode::BinaryNullishCoalescing => {
            is_primitive_literal(binary.left.data.as_deref())
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum SideEffects {
    #[default]
    CouldHaveSideEffects,
    NoSideEffects,
}

#[must_use]
pub fn to_null_or_undefined_with_side_effects(
    data: Option<&ExprData>,
) -> Option<(bool, SideEffects)> {
    match data? {
        ExprData::Annotation(value) => {
            let (is_nullish, mut side_effects) =
                to_null_or_undefined_with_side_effects(value.value.data.as_deref())?;
            if value
                .flags
                .contains(AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED)
            {
                side_effects = SideEffects::NoSideEffects;
            }
            Some((is_nullish, side_effects))
        }
        ExprData::InlinedEnum(value) => {
            to_null_or_undefined_with_side_effects(value.value.data.as_deref())
        }
        ExprData::Boolean(_)
        | ExprData::Number(_)
        | ExprData::String(_)
        | ExprData::RegExp(_)
        | ExprData::Function(_)
        | ExprData::Arrow(_)
        | ExprData::BigInt(_) => Some((false, SideEffects::NoSideEffects)),
        ExprData::Object(_) | ExprData::Array(_) | ExprData::Class(_) => {
            Some((false, SideEffects::CouldHaveSideEffects))
        }
        ExprData::Null | ExprData::Undefined => Some((true, SideEffects::NoSideEffects)),
        ExprData::Unary(value) => match value.op {
            OpCode::UnaryPositive
            | OpCode::UnaryNegative
            | OpCode::UnaryComplement
            | OpCode::UnaryPreDecrement
            | OpCode::UnaryPreIncrement
            | OpCode::UnaryPostDecrement
            | OpCode::UnaryPostIncrement
            | OpCode::UnaryNot
            | OpCode::UnaryDelete => Some((false, SideEffects::CouldHaveSideEffects)),
            OpCode::UnaryTypeof => Some((
                false,
                if value.was_originally_typeof_identifier {
                    SideEffects::NoSideEffects
                } else {
                    SideEffects::CouldHaveSideEffects
                },
            )),
            OpCode::UnaryVoid => Some((true, SideEffects::CouldHaveSideEffects)),
            _ => None,
        },
        ExprData::Binary(value) => match value.op {
            OpCode::BinaryAdd
            | OpCode::BinaryAddAssign
            | OpCode::BinarySubtract
            | OpCode::BinaryMultiply
            | OpCode::BinaryDivide
            | OpCode::BinaryRemainder
            | OpCode::BinaryPower
            | OpCode::BinarySubtractAssign
            | OpCode::BinaryMultiplyAssign
            | OpCode::BinaryDivideAssign
            | OpCode::BinaryRemainderAssign
            | OpCode::BinaryPowerAssign
            | OpCode::BinaryShiftLeft
            | OpCode::BinaryShiftRight
            | OpCode::BinaryUnsignedShiftRight
            | OpCode::BinaryShiftLeftAssign
            | OpCode::BinaryShiftRightAssign
            | OpCode::BinaryUnsignedShiftRightAssign
            | OpCode::BinaryBitwiseOr
            | OpCode::BinaryBitwiseAnd
            | OpCode::BinaryBitwiseXor
            | OpCode::BinaryBitwiseOrAssign
            | OpCode::BinaryBitwiseAndAssign
            | OpCode::BinaryBitwiseXorAssign
            | OpCode::BinaryLessThan
            | OpCode::BinaryLessThanOrEqual
            | OpCode::BinaryGreaterThan
            | OpCode::BinaryGreaterThanOrEqual
            | OpCode::BinaryIn
            | OpCode::BinaryInstanceof
            | OpCode::BinaryLooseEqual
            | OpCode::BinaryLooseNotEqual
            | OpCode::BinaryStrictEqual
            | OpCode::BinaryStrictNotEqual => Some((false, SideEffects::CouldHaveSideEffects)),
            OpCode::BinaryComma => {
                to_null_or_undefined_with_side_effects(value.right.data.as_deref())
                    .map(|(is_nullish, _)| (is_nullish, SideEffects::CouldHaveSideEffects))
            }
            _ => None,
        },
        _ => None,
    }
}

#[must_use]
#[allow(clippy::float_cmp)]
pub fn to_boolean_with_side_effects(data: Option<&ExprData>) -> Option<(bool, SideEffects)> {
    match data? {
        ExprData::Annotation(value) => {
            let (boolean, mut side_effects) =
                to_boolean_with_side_effects(value.value.data.as_deref())?;
            if value
                .flags
                .contains(AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED)
            {
                side_effects = SideEffects::NoSideEffects;
            }
            Some((boolean, side_effects))
        }
        ExprData::InlinedEnum(value) => to_boolean_with_side_effects(value.value.data.as_deref()),
        ExprData::Null | ExprData::Undefined => Some((false, SideEffects::NoSideEffects)),
        ExprData::Boolean(value) => Some((*value, SideEffects::NoSideEffects)),
        ExprData::Number(value) => {
            Some((*value != 0.0 && !value.is_nan(), SideEffects::NoSideEffects))
        }
        ExprData::BigInt(value) => {
            check_equality_big_int(value, "0").map(|equal| (!equal, SideEffects::NoSideEffects))
        }
        ExprData::String(value) => Some((!value.value.is_empty(), SideEffects::NoSideEffects)),
        ExprData::Function(_) | ExprData::Arrow(_) | ExprData::RegExp(_) => {
            Some((true, SideEffects::NoSideEffects))
        }
        ExprData::Object(_) | ExprData::Array(_) | ExprData::Class(_) => {
            Some((true, SideEffects::CouldHaveSideEffects))
        }
        ExprData::Unary(value) => match value.op {
            OpCode::UnaryVoid => Some((false, SideEffects::CouldHaveSideEffects)),
            OpCode::UnaryTypeof => Some((
                true,
                if value.was_originally_typeof_identifier {
                    SideEffects::NoSideEffects
                } else {
                    SideEffects::CouldHaveSideEffects
                },
            )),
            OpCode::UnaryNot => to_boolean_with_side_effects(value.value.data.as_deref())
                .map(|(boolean, side_effects)| (!boolean, side_effects)),
            _ => None,
        },
        ExprData::Binary(value) => match value.op {
            OpCode::BinaryLogicalOr => to_boolean_with_side_effects(value.right.data.as_deref())
                .and_then(|(boolean, _)| {
                    boolean.then_some((true, SideEffects::CouldHaveSideEffects))
                }),
            OpCode::BinaryLogicalAnd => to_boolean_with_side_effects(value.right.data.as_deref())
                .and_then(|(boolean, _)| {
                    (!boolean).then_some((false, SideEffects::CouldHaveSideEffects))
                }),
            OpCode::BinaryComma => to_boolean_with_side_effects(value.right.data.as_deref())
                .map(|(boolean, _)| (boolean, SideEffects::CouldHaveSideEffects)),
            _ => None,
        },
        _ => None,
    }
}

#[must_use]
#[allow(clippy::float_cmp, clippy::too_many_lines)]
pub fn fold_binary_operator(loc: Loc, binary: &BinaryExpr) -> Option<Expr> {
    let number = |value| Some(Expr::new(loc, ExprData::Number(value)));
    let boolean = |value| Some(Expr::new(loc, ExprData::Boolean(value)));
    match binary.op {
        OpCode::BinaryAdd => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right) {
                return number(left + right);
            }
            if let Some((left, right)) = extract_string_values(&binary.left, &binary.right) {
                let mut value = Vec::with_capacity(left.len() + right.len());
                value.extend_from_slice(left);
                value.extend_from_slice(right);
                return Some(Expr::new(
                    loc,
                    ExprData::String(super::StringExpr {
                        value,
                        ..super::StringExpr::default()
                    }),
                ));
            }
            None
        }
        OpCode::BinarySubtract => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(left - right)),
        OpCode::BinaryMultiply => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(left * right)),
        OpCode::BinaryDivide => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(left / right)),
        OpCode::BinaryRemainder => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(left % right)),
        OpCode::BinaryPower => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(left.powf(right))),
        OpCode::BinaryShiftLeft => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(f64::from(to_int32(left) << (to_uint32(right) & 31)))),
        OpCode::BinaryShiftRight => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(f64::from(to_int32(left) >> (to_uint32(right) & 31)))),
        OpCode::BinaryUnsignedShiftRight => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| {
                number(f64::from(to_uint32(left) >> (to_uint32(right) & 31)))
            }),
        OpCode::BinaryBitwiseAnd => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(f64::from(to_int32(left) & to_int32(right)))),
        OpCode::BinaryBitwiseOr => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(f64::from(to_int32(left) | to_int32(right)))),
        OpCode::BinaryBitwiseXor => extract_numeric_values(&binary.left, &binary.right)
            .and_then(|(left, right)| number(f64::from(to_int32(left) ^ to_int32(right)))),
        OpCode::BinaryLessThan => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right) {
                return boolean(left < right);
            }
            extract_string_values(&binary.left, &binary.right)
                .and_then(|(left, right)| boolean(string_compare_ucs2(left, right) < 0))
        }
        OpCode::BinaryGreaterThan => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right) {
                return boolean(left > right);
            }
            extract_string_values(&binary.left, &binary.right)
                .and_then(|(left, right)| boolean(string_compare_ucs2(left, right) > 0))
        }
        OpCode::BinaryLessThanOrEqual => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right) {
                return boolean(left <= right);
            }
            extract_string_values(&binary.left, &binary.right)
                .and_then(|(left, right)| boolean(string_compare_ucs2(left, right) <= 0))
        }
        OpCode::BinaryGreaterThanOrEqual => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right) {
                return boolean(left >= right);
            }
            extract_string_values(&binary.left, &binary.right)
                .and_then(|(left, right)| boolean(string_compare_ucs2(left, right) >= 0))
        }
        OpCode::BinaryLooseEqual | OpCode::BinaryStrictEqual => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right) {
                return boolean(left == right);
            }
            extract_string_values(&binary.left, &binary.right)
                .and_then(|(left, right)| boolean(string_compare_ucs2(left, right) == 0))
        }
        OpCode::BinaryLooseNotEqual | OpCode::BinaryStrictNotEqual => {
            if let Some((left, right)) = extract_numeric_values(&binary.left, &binary.right) {
                return boolean(left != right);
            }
            extract_string_values(&binary.left, &binary.right)
                .and_then(|(left, right)| boolean(string_compare_ucs2(left, right) != 0))
        }
        OpCode::BinaryLogicalAnd => {
            let (boolean, side_effects) =
                to_boolean_with_side_effects(binary.left.data.as_deref())?;
            if !boolean {
                Some(binary.left.clone())
            } else if side_effects == SideEffects::NoSideEffects {
                Some(binary.right.clone())
            } else {
                None
            }
        }
        OpCode::BinaryLogicalOr => {
            let (boolean, side_effects) =
                to_boolean_with_side_effects(binary.left.data.as_deref())?;
            if boolean {
                Some(binary.left.clone())
            } else if side_effects == SideEffects::NoSideEffects {
                Some(binary.right.clone())
            } else {
                None
            }
        }
        OpCode::BinaryNullishCoalescing => {
            let (is_nullish, side_effects) =
                to_null_or_undefined_with_side_effects(binary.left.data.as_deref())?;
            if !is_nullish {
                Some(binary.left.clone())
            } else if side_effects == SideEffects::NoSideEffects {
                Some(binary.right.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn simplify_boolean_expr(
    expr: &Expr,
    expr_can_be_removed_if_unused: &dyn Fn(&Expr) -> bool,
) -> Expr {
    match expr.data.as_deref() {
        Some(ExprData::Unary(unary)) if unary.op == OpCode::UnaryNot => {
            if let Some(ExprData::Unary(inner)) = unary.value.data.as_deref()
                && inner.op == OpCode::UnaryNot
            {
                return simplify_boolean_expr(&inner.value, expr_can_be_removed_if_unused);
            }
            Expr::new(
                expr.loc,
                ExprData::Unary(UnaryExpr {
                    value: simplify_boolean_expr(&unary.value, expr_can_be_removed_if_unused),
                    ..unary.clone()
                }),
            )
        }
        Some(ExprData::Binary(binary)) => {
            let mut left = binary.left.clone();
            let mut right = binary.right.clone();
            match binary.op {
                OpCode::BinaryStrictEqual
                | OpCode::BinaryStrictNotEqual
                | OpCode::BinaryLooseEqual
                | OpCode::BinaryLooseNotEqual => {
                    if extract_numeric_value(right.data.as_deref()) == Some(0.0)
                        && is_int32_or_uint32(left.data.as_deref())
                    {
                        return if matches!(
                            binary.op,
                            OpCode::BinaryStrictNotEqual | OpCode::BinaryLooseNotEqual
                        ) {
                            left
                        } else {
                            not(left)
                        };
                    }
                }
                OpCode::BinaryLogicalAnd => {
                    left = simplify_boolean_expr(&left, expr_can_be_removed_if_unused);
                    right = simplify_boolean_expr(&right, expr_can_be_removed_if_unused);
                    if matches!(
                        to_boolean_with_side_effects(right.data.as_deref()),
                        Some((true, SideEffects::NoSideEffects))
                    ) {
                        return left;
                    }
                }
                OpCode::BinaryLogicalOr => {
                    left = simplify_boolean_expr(&left, expr_can_be_removed_if_unused);
                    right = simplify_boolean_expr(&right, expr_can_be_removed_if_unused);
                    if matches!(
                        to_boolean_with_side_effects(right.data.as_deref()),
                        Some((false, SideEffects::NoSideEffects))
                    ) {
                        return left;
                    }
                }
                _ => {}
            }
            Expr::new(
                expr.loc,
                ExprData::Binary(BinaryExpr {
                    left,
                    right,
                    ..binary.clone()
                }),
            )
        }
        Some(ExprData::If(value)) => {
            let yes = simplify_boolean_expr(&value.yes, expr_can_be_removed_if_unused);
            let no = simplify_boolean_expr(&value.no, expr_can_be_removed_if_unused);
            if let Some((boolean, SideEffects::NoSideEffects)) =
                to_boolean_with_side_effects(yes.data.as_deref())
            {
                return if boolean {
                    join_with_left_associative_op(OpCode::BinaryLogicalOr, value.test.clone(), no)
                } else {
                    join_with_left_associative_op(
                        OpCode::BinaryLogicalAnd,
                        not(value.test.clone()),
                        no,
                    )
                };
            }
            if let Some((boolean, SideEffects::NoSideEffects)) =
                to_boolean_with_side_effects(no.data.as_deref())
            {
                return if boolean {
                    join_with_left_associative_op(
                        OpCode::BinaryLogicalOr,
                        not(value.test.clone()),
                        yes,
                    )
                } else {
                    join_with_left_associative_op(OpCode::BinaryLogicalAnd, value.test.clone(), yes)
                };
            }
            Expr::new(
                expr.loc,
                ExprData::If(super::IfExpr {
                    test: value.test.clone(),
                    yes,
                    no,
                }),
            )
        }
        data => {
            if let Some((boolean, side_effects)) = to_boolean_with_side_effects(data)
                && (side_effects == SideEffects::NoSideEffects
                    || expr_can_be_removed_if_unused(expr))
            {
                return Expr::new(expr.loc, ExprData::Boolean(boolean));
            }
            expr.clone()
        }
    }
}

fn simplify_unused_string_addition_chain(expr: &Expr) -> (Expr, bool) {
    match expr.data.as_deref() {
        Some(ExprData::String(_)) => (
            Expr::new(expr.loc, ExprData::String(super::StringExpr::default())),
            true,
        ),
        Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryAdd => {
            let (left, left_is_string_addition) =
                simplify_unused_string_addition_chain(&binary.left);
            if let Some(ExprData::String(right)) = binary.right.data.as_deref() {
                if left_is_string_addition {
                    return (left, true);
                }
                if !right.value.is_empty() {
                    return (
                        Expr::new(
                            expr.loc,
                            ExprData::Binary(BinaryExpr {
                                left,
                                right: Expr::new(
                                    binary.right.loc,
                                    ExprData::String(super::StringExpr::default()),
                                ),
                                op: OpCode::BinaryAdd,
                            }),
                        ),
                        true,
                    );
                }
            }
            (
                Expr::new(
                    expr.loc,
                    ExprData::Binary(BinaryExpr {
                        left,
                        right: binary.right.clone(),
                        ..binary.clone()
                    }),
                ),
                left_is_string_addition,
            )
        }
        _ => (expr.clone(), false),
    }
}

#[must_use]
pub fn is_binary_null_and_undefined(left: &Expr, right: &Expr, op: OpCode) -> Option<(Expr, Expr)> {
    let Some(ExprData::Binary(left_binary)) = left.data.as_deref() else {
        return None;
    };
    let Some(ExprData::Binary(right_binary)) = right.data.as_deref() else {
        return None;
    };
    if left_binary.op != op || right_binary.op != op {
        return None;
    }

    let (mut left_id, mut left_equality) = (&left_binary.left, &left_binary.right);
    let (mut right_id, mut right_equality) = (&right_binary.left, &right_binary.right);
    if matches!(left_equality.data.as_deref(), Some(ExprData::Identifier(_))) {
        std::mem::swap(&mut left_id, &mut left_equality);
    }
    if matches!(
        right_equality.data.as_deref(),
        Some(ExprData::Identifier(_))
    ) {
        std::mem::swap(&mut right_id, &mut right_equality);
    }

    let (Some(ExprData::Identifier(left_identifier)), Some(ExprData::Identifier(right_identifier))) =
        (left_id.data.as_deref(), right_id.data.as_deref())
    else {
        return None;
    };
    if left_identifier.reference != right_identifier.reference {
        return None;
    }

    if matches!(left_equality.data.as_deref(), Some(ExprData::Null))
        && matches!(right_equality.data.as_deref(), Some(ExprData::Undefined))
    {
        return Some((left_binary.left.clone(), left_binary.right.clone()));
    }
    if matches!(left_equality.data.as_deref(), Some(ExprData::Undefined))
        && matches!(right_equality.data.as_deref(), Some(ExprData::Null))
    {
        return Some((right_binary.left.clone(), right_binary.right.clone()));
    }
    None
}

#[must_use]
pub fn check_equality_big_int(left: &str, right: &str) -> Option<bool> {
    if left == right {
        return Some(true);
    }
    if (left.len() < 2 || !left.starts_with('0')) && (right.len() < 2 || !right.starts_with('0')) {
        return Some(false);
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EqualityKind {
    Loose,
    Strict,
}

#[must_use]
#[allow(clippy::float_cmp, clippy::too_many_lines)]
pub fn check_equality_if_no_side_effects(
    left: Option<&ExprData>,
    right: Option<&ExprData>,
    kind: EqualityKind,
) -> Option<bool> {
    if let Some(ExprData::InlinedEnum(value)) = right {
        return check_equality_if_no_side_effects(left, value.value.data.as_deref(), kind);
    }

    match left? {
        ExprData::InlinedEnum(value) => {
            check_equality_if_no_side_effects(value.value.data.as_deref(), right, kind)
        }
        ExprData::Null => match right? {
            ExprData::Null => Some(true),
            ExprData::Undefined => Some(kind == EqualityKind::Loose),
            right if is_primitive_literal(Some(right)) => Some(false),
            _ => None,
        },
        ExprData::Undefined => match right? {
            ExprData::Undefined => Some(true),
            ExprData::Null => Some(kind == EqualityKind::Loose),
            right if is_primitive_literal(Some(right)) => Some(false),
            _ => None,
        },
        ExprData::Boolean(left) => match right? {
            ExprData::Boolean(right) => Some(left == right),
            ExprData::Number(right) if kind == EqualityKind::Loose => {
                Some(*right == if *left { 1.0 } else { 0.0 })
            }
            ExprData::Number(_) | ExprData::Null | ExprData::Undefined => Some(false),
            right if kind == EqualityKind::Strict && is_primitive_literal(Some(right)) => {
                Some(false)
            }
            _ => None,
        },
        ExprData::Number(left) => match right? {
            ExprData::Number(right) => Some(left == right),
            ExprData::Boolean(right) if kind == EqualityKind::Loose => {
                Some(*left == if *right { 1.0 } else { 0.0 })
            }
            ExprData::Boolean(_) | ExprData::Null | ExprData::Undefined => Some(false),
            right if kind == EqualityKind::Strict && is_primitive_literal(Some(right)) => {
                Some(false)
            }
            _ => None,
        },
        ExprData::BigInt(left) => match right? {
            ExprData::BigInt(right) => check_equality_big_int(left, right),
            ExprData::Null | ExprData::Undefined => Some(false),
            right if kind == EqualityKind::Strict && is_primitive_literal(Some(right)) => {
                Some(false)
            }
            _ => None,
        },
        ExprData::String(left) => match right? {
            ExprData::String(right) => Some(left.value == right.value),
            ExprData::Null | ExprData::Undefined => Some(false),
            right if kind == EqualityKind::Strict && is_primitive_literal(Some(right)) => {
                Some(false)
            }
            _ => None,
        },
        _ => None,
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn values_look_the_same(left: Option<&ExprData>, right: Option<&ExprData>) -> bool {
    if let Some(ExprData::InlinedEnum(value)) = right {
        return values_look_the_same(left, value.value.data.as_deref());
    }

    match left {
        Some(ExprData::InlinedEnum(value)) => {
            return values_look_the_same(value.value.data.as_deref(), right);
        }
        Some(ExprData::Identifier(left)) => {
            if let Some(ExprData::Identifier(right)) = right
                && left.reference == right.reference
            {
                return true;
            }
        }
        Some(ExprData::Dot(left)) => {
            if let Some(ExprData::Dot(right)) = right
                && left.has_same_flags_as(right)
                && left.name == right.name
                && values_look_the_same(left.target.data.as_deref(), right.target.data.as_deref())
            {
                return true;
            }
        }
        Some(ExprData::Index(left)) => {
            if let Some(ExprData::Index(right)) = right
                && left.has_same_flags_as(right)
                && values_look_the_same(left.target.data.as_deref(), right.target.data.as_deref())
                && values_look_the_same(left.index.data.as_deref(), right.index.data.as_deref())
            {
                return true;
            }
        }
        Some(ExprData::If(left)) => {
            if let Some(ExprData::If(right)) = right
                && values_look_the_same(left.test.data.as_deref(), right.test.data.as_deref())
                && values_look_the_same(left.yes.data.as_deref(), right.yes.data.as_deref())
                && values_look_the_same(left.no.data.as_deref(), right.no.data.as_deref())
            {
                return true;
            }
        }
        Some(ExprData::Unary(left)) => {
            if let Some(ExprData::Unary(right)) = right
                && left.op == right.op
                && values_look_the_same(left.value.data.as_deref(), right.value.data.as_deref())
            {
                return true;
            }
        }
        Some(ExprData::Binary(left)) => {
            if let Some(ExprData::Binary(right)) = right
                && left.op == right.op
                && values_look_the_same(left.left.data.as_deref(), right.left.data.as_deref())
                && values_look_the_same(left.right.data.as_deref(), right.right.data.as_deref())
            {
                return true;
            }
        }
        Some(ExprData::Call(left)) => {
            if let Some(ExprData::Call(right)) = right
                && left.has_same_flags_as(right)
                && left.args.len() == right.args.len()
                && values_look_the_same(left.target.data.as_deref(), right.target.data.as_deref())
                && left.args.iter().zip(&right.args).all(|(left, right)| {
                    values_look_the_same(left.data.as_deref(), right.data.as_deref())
                })
            {
                return true;
            }
        }
        Some(ExprData::Number(left))
            if matches!(right, Some(ExprData::Number(right)) if *left == 0.0
                && *right == 0.0
                && left.is_sign_negative() != right.is_sign_negative()) =>
        {
            return false;
        }
        _ => {}
    }

    check_equality_if_no_side_effects(left, right, EqualityKind::Strict) == Some(true)
}

pub fn try_to_insert_optional_chain(test: &Expr, expr: &mut Expr) -> bool {
    match expr.data.as_deref_mut() {
        Some(ExprData::Dot(value)) => {
            if values_look_the_same(test.data.as_deref(), value.target.data.as_deref()) {
                value.optional_chain = OptionalChain::Start;
                return true;
            }
            if try_to_insert_optional_chain(test, &mut value.target) {
                if value.optional_chain == OptionalChain::None {
                    value.optional_chain = OptionalChain::Continue;
                }
                return true;
            }
        }
        Some(ExprData::Index(value)) => {
            if values_look_the_same(test.data.as_deref(), value.target.data.as_deref()) {
                value.optional_chain = OptionalChain::Start;
                return true;
            }
            if try_to_insert_optional_chain(test, &mut value.target) {
                if value.optional_chain == OptionalChain::None {
                    value.optional_chain = OptionalChain::Continue;
                }
                return true;
            }
        }
        Some(ExprData::Call(value)) => {
            if values_look_the_same(test.data.as_deref(), value.target.data.as_deref()) {
                value.optional_chain = OptionalChain::Start;
                return true;
            }
            if try_to_insert_optional_chain(test, &mut value.target) {
                if value.optional_chain == OptionalChain::None {
                    value.optional_chain = OptionalChain::Continue;
                }
                return true;
            }
        }
        _ => {}
    }
    false
}

#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
pub fn try_to_string_on_number_safely(value: f64, radix: u32) -> Option<String> {
    let integer = value as i32;
    if f64::from(integer) == value {
        return format_i32_radix(integer, radix);
    }
    if value.is_nan() {
        return Some("NaN".into());
    }
    if value == f64::INFINITY {
        return Some("Infinity".into());
    }
    if value == f64::NEG_INFINITY {
        return Some("-Infinity".into());
    }
    None
}

fn fold_addition_pre_process(mut expr: Expr) -> Expr {
    match expr.data.as_deref() {
        Some(ExprData::InlinedEnum(value)) => value.value.clone(),
        Some(ExprData::Array(array)) => {
            let mut joined = Vec::new();
            for (index, item) in array.items.iter().enumerate() {
                if index > 0 {
                    joined.push(u16::from(b','));
                }
                match item.data.as_deref() {
                    Some(ExprData::Undefined | ExprData::Null) => {}
                    Some(ExprData::String(value)) => joined.extend_from_slice(&value.value),
                    data => {
                        let Some(value) = to_string_without_side_effects(data) else {
                            return expr;
                        };
                        joined.extend(value.encode_utf16());
                    }
                }
            }
            expr.data = Some(Box::new(ExprData::String(super::StringExpr {
                value: joined,
                ..super::StringExpr::default()
            })));
            expr
        }
        Some(ExprData::Object(object)) if object.properties.is_empty() => {
            expr.data = Some(Box::new(ExprData::String(super::StringExpr {
                value: "[object Object]".encode_utf16().collect(),
                ..super::StringExpr::default()
            })));
            expr
        }
        _ => expr,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum StringAdditionKind {
    #[default]
    Normal,
    WithNestedLeft,
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn fold_string_addition(left: Expr, right: Expr, kind: StringAdditionKind) -> Option<Expr> {
    let mut left = fold_addition_pre_process(left);
    let mut right = fold_addition_pre_process(right);

    if kind != StringAdditionKind::WithNestedLeft
        && matches!(
            right.data.as_deref(),
            Some(ExprData::String(_) | ExprData::Template(_))
        )
        && let Some(string) = to_string_without_side_effects(left.data.as_deref())
    {
        left.data = Some(Box::new(ExprData::String(super::StringExpr {
            value: string.encode_utf16().collect(),
            ..super::StringExpr::default()
        })));
    }

    match left.data.as_deref() {
        Some(ExprData::String(left_string)) => {
            if let Some(string) = to_string_without_side_effects(right.data.as_deref()) {
                right.data = Some(Box::new(ExprData::String(super::StringExpr {
                    value: string.encode_utf16().collect(),
                    ..super::StringExpr::default()
                })));
            }

            match right.data.as_deref() {
                Some(ExprData::String(right_string)) => {
                    let mut value =
                        Vec::with_capacity(left_string.value.len() + right_string.value.len());
                    value.extend_from_slice(&left_string.value);
                    value.extend_from_slice(&right_string.value);
                    return Some(Expr::new(
                        left.loc,
                        ExprData::String(super::StringExpr {
                            value,
                            prefer_template: left_string.prefer_template
                                || right_string.prefer_template,
                            ..super::StringExpr::default()
                        }),
                    ));
                }
                Some(ExprData::Template(right_template))
                    if right_template.tag_or_nil.data.is_none() =>
                {
                    let mut head_cooked = Vec::with_capacity(
                        left_string.value.len() + right_template.head_cooked.len(),
                    );
                    head_cooked.extend_from_slice(&left_string.value);
                    head_cooked.extend_from_slice(&right_template.head_cooked);
                    return Some(Expr::new(
                        left.loc,
                        ExprData::Template(super::TemplateExpr {
                            head_loc: left.loc,
                            head_cooked,
                            parts: right_template.parts.clone(),
                            ..super::TemplateExpr::default()
                        }),
                    ));
                }
                _ => {}
            }

            if left_string.value.is_empty()
                && known_primitive_type(right.data.as_deref()) == PrimitiveType::String
            {
                return Some(right);
            }
        }
        Some(ExprData::Template(left_template)) if left_template.tag_or_nil.data.is_none() => {
            if let Some(string) = to_string_without_side_effects(right.data.as_deref()) {
                right.data = Some(Box::new(ExprData::String(super::StringExpr {
                    value: string.encode_utf16().collect(),
                    ..super::StringExpr::default()
                })));
            }

            match right.data.as_deref() {
                Some(ExprData::String(right_string)) => {
                    let mut head_cooked = left_template.head_cooked.clone();
                    let mut parts = left_template.parts.clone();
                    if let Some(last) = parts.last_mut() {
                        last.tail_cooked.extend_from_slice(&right_string.value);
                    } else {
                        head_cooked.extend_from_slice(&right_string.value);
                    }
                    return Some(Expr::new(
                        left.loc,
                        ExprData::Template(super::TemplateExpr {
                            head_loc: left_template.head_loc,
                            head_cooked,
                            parts,
                            ..super::TemplateExpr::default()
                        }),
                    ));
                }
                Some(ExprData::Template(right_template))
                    if right_template.tag_or_nil.data.is_none() =>
                {
                    let mut head_cooked = left_template.head_cooked.clone();
                    let mut parts = left_template.parts.clone();
                    if let Some(last) = parts.last_mut() {
                        last.tail_cooked
                            .extend_from_slice(&right_template.head_cooked);
                    } else {
                        head_cooked.extend_from_slice(&right_template.head_cooked);
                    }
                    parts.extend_from_slice(&right_template.parts);
                    return Some(Expr::new(
                        left.loc,
                        ExprData::Template(super::TemplateExpr {
                            head_loc: left_template.head_loc,
                            head_cooked,
                            parts,
                            ..super::TemplateExpr::default()
                        }),
                    ));
                }
                _ => {}
            }
        }
        _ => {}
    }

    if matches!(
        right.data.as_deref(),
        Some(ExprData::String(value)) if value.value.is_empty()
    ) && known_primitive_type(left.data.as_deref()) == PrimitiveType::String
    {
        return Some(left);
    }

    None
}

#[must_use]
pub fn inline_primitives_into_template(loc: Loc, template: &super::TemplateExpr) -> Expr {
    if template.tag_or_nil.data.is_some() {
        return Expr::new(loc, ExprData::Template(template.clone()));
    }

    let mut head_cooked = template.head_cooked.clone();
    let mut parts: Vec<super::TemplatePart> = Vec::with_capacity(template.parts.len());
    for original_part in &template.parts {
        let mut part = original_part.clone();
        if let Some(ExprData::InlinedEnum(value)) = part.value.data.as_deref() {
            part.value = value.value.clone();
        }
        if let Some(string) = to_string_without_side_effects(part.value.data.as_deref()) {
            part.value.data = Some(Box::new(ExprData::String(super::StringExpr {
                value: string.encode_utf16().collect(),
                ..super::StringExpr::default()
            })));
        }
        if let Some(ExprData::String(string)) = part.value.data.as_deref() {
            let destination = parts
                .last_mut()
                .map_or(&mut head_cooked, |previous| &mut previous.tail_cooked);
            destination.extend_from_slice(&string.value);
            destination.extend_from_slice(&part.tail_cooked);
        } else {
            parts.push(part);
        }
    }

    if parts.is_empty() {
        return Expr::new(
            loc,
            ExprData::String(super::StringExpr {
                value: head_cooked,
                prefer_template: true,
                ..super::StringExpr::default()
            }),
        );
    }

    Expr::new(
        loc,
        ExprData::Template(super::TemplateExpr {
            head_loc: template.head_loc,
            head_cooked,
            parts,
            ..super::TemplateExpr::default()
        }),
    )
}

#[must_use]
pub fn string_to_equivalent_number_value(value: &[u16]) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    let negative = value[0] == u16::from(b'-') && value.len() > 1;
    let start = usize::from(negative);
    let mut integer = 0_i32;
    for &character in &value[start..] {
        if !(u16::from(b'0')..=u16::from(b'9')).contains(&character) {
            return None;
        }
        integer = integer
            .wrapping_mul(10)
            .wrapping_add(i32::from(character) - i32::from(b'0'));
    }
    if negative {
        integer = integer.wrapping_neg();
    }
    let printed: Vec<u16> = integer.to_string().encode_utf16().collect();
    (printed == value).then_some(f64::from(integer))
}

#[must_use]
pub fn inline_spreads_of_array_literals(values: &[Expr]) -> Vec<Expr> {
    let mut results = Vec::new();
    for value in values {
        if let Some(ExprData::Spread(spread)) = value.data.as_deref()
            && let Some(ExprData::Array(array)) = spread.value.data.as_deref()
        {
            results.extend(array.items.iter().map(|item| {
                if matches!(item.data.as_deref(), Some(ExprData::Missing)) {
                    Expr::new(item.loc, ExprData::Undefined)
                } else {
                    item.clone()
                }
            }));
            continue;
        }
        results.push(value.clone());
    }
    results
}

#[must_use]
pub fn mangle_object_spread(properties: &[Property]) -> Vec<Property> {
    let mut result = Vec::new();
    for property in properties {
        if property.kind == PropertyKind::Spread {
            match property.value_or_nil.data.as_deref() {
                Some(
                    ExprData::Boolean(_)
                    | ExprData::Null
                    | ExprData::Undefined
                    | ExprData::Number(_)
                    | ExprData::BigInt(_)
                    | ExprData::RegExp(_)
                    | ExprData::Function(_)
                    | ExprData::Arrow(_),
                ) => continue,
                Some(ExprData::Object(object)) => {
                    for (index, nested) in object.properties.iter().enumerate() {
                        let is_accessor =
                            matches!(nested.kind, PropertyKind::Getter | PropertyKind::Setter);
                        let is_proto = nested.kind == PropertyKind::Field
                            && !nested.flags.contains(PropertyFlags::IS_COMPUTED)
                            && matches!(
                                nested.key.data.as_deref(),
                                Some(ExprData::String(value))
                                    if utf16_equals_wtf8(&value.value, b"__proto__")
                            );
                        if is_accessor || is_proto {
                            let mut remaining = property.clone();
                            remaining.value_or_nil = Expr::new(
                                property.value_or_nil.loc,
                                ExprData::Object(ObjectExpr {
                                    properties: object.properties[index..].to_vec(),
                                    ..object.clone()
                                }),
                            );
                            result.push(remaining);
                            break;
                        }
                        result.push(nested.clone());
                    }
                    continue;
                }
                _ => {}
            }
        }
        result.push(property.clone());
    }
    result
}

pub fn for_each_identifier_binding_in_decls(
    declarations: &mut [super::Decl],
    callback: &mut impl FnMut(Loc, &mut super::IdentifierBinding),
) {
    for declaration in declarations {
        for_each_identifier_binding(&mut declaration.binding, callback);
    }
}

/// # Panics
///
/// Panics if the binding is an invalid placeholder without binding data.
pub fn for_each_identifier_binding(
    binding: &mut Binding,
    callback: &mut impl FnMut(Loc, &mut super::IdentifierBinding),
) {
    let data = binding
        .data
        .as_deref_mut()
        .expect("internal error: missing binding data");
    match data {
        BindingData::Missing => {}
        BindingData::Identifier(identifier) => callback(binding.loc, identifier),
        BindingData::Array(array) => {
            for item in &mut array.items {
                for_each_identifier_binding(&mut item.binding, callback);
            }
        }
        BindingData::Object(object) => {
            for property in &mut object.properties {
                for_each_identifier_binding(&mut property.value, callback);
            }
        }
    }
}

fn format_i32_radix(value: i32, radix: u32) -> Option<String> {
    if !(2..=36).contains(&radix) {
        return None;
    }
    if value == 0 {
        return Some("0".into());
    }
    let negative = value < 0;
    let mut value = value.unsigned_abs();
    let mut digits = Vec::new();
    while value > 0 {
        let digit = value % radix;
        digits.push(if digit < 10 {
            char::from(b'0' + u8::try_from(digit).ok()?)
        } else {
            char::from(b'a' + u8::try_from(digit - 10).ok()?)
        });
        value /= radix;
    }
    if negative {
        digits.push('-');
    }
    digits.reverse();
    Some(digits.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::{
        EqualityKind, PrimitiveType, SideEffects, StmtsCanBeRemovedIfUnusedFlags,
        StringAdditionKind, assign, can_change_strict_to_loose, check_equality_big_int,
        check_equality_if_no_side_effects, convert_binding_to_expr, fold_binary_operator,
        fold_string_addition, for_each_identifier_binding, inline_primitives_into_template,
        inline_spreads_of_array_literals, is_binary_null_and_undefined, is_optional_chain,
        join_all_with_comma, known_primitive_type, make_helper_context, mangle_object_spread,
        maybe_simplify_equality_comparison, maybe_simplify_not, not,
        should_fold_binary_operator_when_minifying, simplify_boolean_expr, string_compare_ucs2,
        string_to_equivalent_number_value, to_boolean_with_side_effects, to_int32,
        to_null_or_undefined_with_side_effects, to_number_without_side_effects,
        to_string_without_side_effects, to_uint32, try_to_insert_optional_chain,
        try_to_string_on_number_safely, typeof_without_side_effects, values_look_the_same,
    };
    use crate::internal::ast::Ref;
    use crate::internal::compat::JsFeature;
    use crate::internal::js_ast::{
        ArrayBinding, ArrayBindingPattern, ArrayExpr, BinaryExpr, Binding, BindingData, CallExpr,
        Class, Decl, Expr, ExprData, ExprStmt, Function, FunctionExpr, IdentifierBinding,
        IdentifierExpr, IfExpr, LocalKind, LocalStmt, ObjectBindingPattern, ObjectExpr, OpCode,
        OptionalChain, Property, PropertyBinding, PropertyFlags, PropertyKind, ReturnStmt,
        SpreadExpr, Stmt, StmtData, StringExpr, TemplateExpr, TemplatePart, UnaryExpr,
    };
    use crate::internal::logger::Loc;

    fn number(value: f64) -> Expr {
        Expr::new(Loc::default(), ExprData::Number(value))
    }

    #[test]
    fn simplifies_not_and_classifies_primitives() {
        assert!(matches!(
            maybe_simplify_not(&number(0.0))
                .expect("simplified")
                .data
                .as_deref(),
            Some(ExprData::Boolean(true))
        ));
        let comparison = Expr::new(
            Loc::default(),
            ExprData::Binary(BinaryExpr {
                left: number(1.0),
                right: number(2.0),
                op: OpCode::BinaryStrictEqual,
            }),
        );
        assert!(matches!(
            not(comparison).data.as_deref(),
            Some(ExprData::Binary(BinaryExpr {
                op: OpCode::BinaryStrictNotEqual,
                ..
            }))
        ));
        assert_eq!(
            known_primitive_type(
                Expr::new(Loc::default(), ExprData::String(StringExpr::default()))
                    .data
                    .as_deref()
            ),
            PrimitiveType::String
        );
        assert!(can_change_strict_to_loose(&number(1.0), &number(2.0)));
        assert_eq!(
            typeof_without_side_effects(number(1.0).data.as_deref()),
            Some("number")
        );
    }

    #[test]
    fn joins_comma_chains_and_converts_array_bindings() {
        let joined = join_all_with_comma([number(1.0), number(2.0), number(3.0)]);
        assert!(matches!(
            joined.data.as_deref(),
            Some(ExprData::Binary(BinaryExpr {
                op: OpCode::BinaryComma,
                ..
            }))
        ));

        let identifier = Binding {
            data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                reference: Ref {
                    source_index: 1,
                    inner_index: 2,
                },
            }))),
            ..Binding::default()
        };
        let binding = Binding {
            data: Some(Box::new(BindingData::Array(ArrayBindingPattern {
                items: vec![ArrayBinding {
                    binding: identifier,
                    ..ArrayBinding::default()
                }],
                has_spread: true,
                ..ArrayBindingPattern::default()
            }))),
            ..Binding::default()
        };
        assert!(matches!(
            convert_binding_to_expr(&binding, None).data.as_deref(),
            Some(ExprData::Array(value))
                if matches!(value.items[0].data.as_deref(), Some(ExprData::Spread(_)))
        ));

        let optional = crate::internal::js_ast::DotExpr {
            optional_chain: OptionalChain::Start,
            ..crate::internal::js_ast::DotExpr::default()
        };
        assert!(is_optional_chain(&Expr::new(
            Loc::default(),
            ExprData::Dot(optional)
        )));
        assert_eq!(assign(number(1.0), number(2.0)).loc, Loc::default());
    }

    #[test]
    fn converts_javascript_numbers_with_int32_wraparound() {
        assert_eq!(to_int32(f64::NAN), 0);
        assert_eq!(to_int32(f64::INFINITY), 0);
        assert_eq!(to_int32(4_294_967_297.0), 1);
        assert_eq!(to_int32(-1.0), -1);
        assert_eq!(to_uint32(-1.0), u32::MAX);
    }

    #[test]
    fn converts_literal_values_without_side_effects() {
        assert_eq!(
            to_number_without_side_effects(
                Expr::new(Loc::default(), ExprData::Null).data.as_deref()
            ),
            Some(0.0)
        );
        assert!(
            to_number_without_side_effects(
                Expr::new(Loc::default(), ExprData::Undefined)
                    .data
                    .as_deref()
            )
            .is_some_and(f64::is_nan)
        );
        assert_eq!(
            to_number_without_side_effects(
                Expr::new(Loc::default(), ExprData::Array(ArrayExpr::default()))
                    .data
                    .as_deref()
            ),
            Some(0.0)
        );
        assert!(
            to_number_without_side_effects(
                Expr::new(Loc::default(), ExprData::Object(ObjectExpr::default()))
                    .data
                    .as_deref()
            )
            .is_some_and(f64::is_nan)
        );
        assert_eq!(
            to_string_without_side_effects(number(f64::INFINITY).data.as_deref()),
            Some("Infinity".into())
        );
    }

    #[test]
    fn recognizes_only_canonical_integer_strings() {
        let utf16 = |text: &str| text.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            string_to_equivalent_number_value(&utf16("123")),
            Some(123.0)
        );
        assert_eq!(
            string_to_equivalent_number_value(&utf16("-12")),
            Some(-12.0)
        );
        assert_eq!(string_to_equivalent_number_value(&utf16("01")), None);
        assert_eq!(string_to_equivalent_number_value(&utf16("-0")), None);
        assert_eq!(string_to_equivalent_number_value(&[]), None);
        assert!(string_compare_ucs2(&utf16("a"), &utf16("b")) < 0);
        assert!(string_compare_ucs2(&utf16("aa"), &utf16("a")) > 0);
    }

    #[test]
    fn safely_formats_numbers_and_compares_bigints() {
        assert_eq!(try_to_string_on_number_safely(255.0, 16), Some("ff".into()));
        assert_eq!(
            try_to_string_on_number_safely(-10.0, 2),
            Some("-1010".into())
        );
        assert_eq!(
            try_to_string_on_number_safely(f64::NAN, 10),
            Some("NaN".into())
        );
        assert_eq!(try_to_string_on_number_safely(1.5, 10), None);
        assert_eq!(check_equality_big_int("1", "1"), Some(true));
        assert_eq!(check_equality_big_int("1", "2"), Some(false));
        assert_eq!(check_equality_big_int("0x1", "1"), None);
        assert!(matches!(
            maybe_simplify_not(&Expr::new(Loc::default(), ExprData::BigInt("0".into())))
                .and_then(|expr| expr.data.map(|data| *data)),
            Some(ExprData::Boolean(true))
        ));
    }

    #[test]
    fn inlines_array_and_object_spreads_without_mutating_inputs() {
        let missing = Expr::new(Loc::default(), ExprData::Missing);
        let spread = Expr::new(
            Loc::default(),
            ExprData::Spread(SpreadExpr {
                value: Expr::new(
                    Loc::default(),
                    ExprData::Array(ArrayExpr {
                        items: vec![missing.clone(), number(1.0)],
                        ..ArrayExpr::default()
                    }),
                ),
            }),
        );
        let inlined = inline_spreads_of_array_literals(&[spread]);
        assert!(matches!(
            inlined[0].data.as_deref(),
            Some(ExprData::Undefined)
        ));
        assert!(matches!(
            inlined[1].data.as_deref(),
            Some(ExprData::Number(1.0))
        ));
        assert!(matches!(missing.data.as_deref(), Some(ExprData::Missing)));

        let field = Property {
            key: Expr::new(
                Loc::default(),
                ExprData::String(StringExpr {
                    value: "x".encode_utf16().collect(),
                    ..StringExpr::default()
                }),
            ),
            value_or_nil: number(1.0),
            ..Property::default()
        };
        let getter = Property {
            kind: PropertyKind::Getter,
            ..Property::default()
        };
        let object = ObjectExpr {
            properties: vec![field.clone(), getter],
            ..ObjectExpr::default()
        };
        let spread_property = Property {
            kind: PropertyKind::Spread,
            value_or_nil: Expr::new(Loc::default(), ExprData::Object(object.clone())),
            ..Property::default()
        };
        let mangled = mangle_object_spread(&[
            Property {
                kind: PropertyKind::Spread,
                value_or_nil: number(1.0),
                ..Property::default()
            },
            spread_property,
        ]);
        assert_eq!(mangled.len(), 2);
        assert_eq!(mangled[0].kind, PropertyKind::Field);
        assert_eq!(mangled[1].kind, PropertyKind::Spread);
        assert!(matches!(
            mangled[1].value_or_nil.data.as_deref(),
            Some(ExprData::Object(value)) if value.properties.len() == 1
        ));
        assert_eq!(object.properties.len(), 2);
    }

    #[test]
    fn checks_literal_equality_without_side_effects() {
        let null = ExprData::Null;
        let undefined = ExprData::Undefined;
        let one = ExprData::Number(1.0);
        let true_value = ExprData::Boolean(true);
        assert_eq!(
            check_equality_if_no_side_effects(Some(&null), Some(&undefined), EqualityKind::Loose),
            Some(true)
        );
        assert_eq!(
            check_equality_if_no_side_effects(Some(&null), Some(&undefined), EqualityKind::Strict),
            Some(false)
        );
        assert_eq!(
            check_equality_if_no_side_effects(Some(&one), Some(&true_value), EqualityKind::Loose),
            Some(true)
        );
        assert_eq!(
            check_equality_if_no_side_effects(Some(&one), Some(&true_value), EqualityKind::Strict),
            Some(false)
        );
    }

    #[test]
    fn compares_structural_values_and_distinguishes_negative_zero() {
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference: Ref {
                    source_index: 3,
                    inner_index: 4,
                },
                ..IdentifierExpr::default()
            }),
        );
        assert!(values_look_the_same(
            identifier.data.as_deref(),
            identifier.clone().data.as_deref()
        ));
        assert!(!values_look_the_same(
            number(-0.0).data.as_deref(),
            number(0.0).data.as_deref()
        ));
        assert!(values_look_the_same(
            number(0.0).data.as_deref(),
            number(0.0).data.as_deref()
        ));
    }

    #[test]
    fn simplifies_boolean_and_typeof_equality_comparisons() {
        let boolean_value = Expr::new(
            Loc::default(),
            ExprData::Unary(UnaryExpr {
                value: number(1.0),
                op: OpCode::UnaryNot,
                ..UnaryExpr::default()
            }),
        );
        let simplified = maybe_simplify_equality_comparison(
            Loc::default(),
            &BinaryExpr {
                left: boolean_value,
                right: Expr::new(Loc::default(), ExprData::Boolean(false)),
                op: OpCode::BinaryStrictEqual,
            },
            JsFeature::NONE,
        )
        .expect("boolean comparison should simplify");
        assert!(matches!(
            simplified.data.as_deref(),
            Some(ExprData::Unary(UnaryExpr {
                op: OpCode::UnaryNot,
                ..
            }))
        ));

        let typeof_value = Expr::new(
            Loc::default(),
            ExprData::Unary(UnaryExpr {
                value: number(1.0),
                op: OpCode::UnaryTypeof,
                ..UnaryExpr::default()
            }),
        );
        let undefined = Expr::new(
            Loc::default(),
            ExprData::String(StringExpr {
                value: "undefined".encode_utf16().collect(),
                ..StringExpr::default()
            }),
        );
        let simplified = maybe_simplify_equality_comparison(
            Loc::default(),
            &BinaryExpr {
                left: typeof_value,
                right: undefined,
                op: OpCode::BinaryLooseNotEqual,
            },
            JsFeature::NONE,
        )
        .expect("typeof comparison should simplify");
        assert!(matches!(
            simplified.data.as_deref(),
            Some(ExprData::Binary(BinaryExpr {
                op: OpCode::BinaryLessThan,
                ..
            }))
        ));
    }

    #[test]
    fn inserts_optional_chain_at_matching_target() {
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference: Ref {
                    source_index: 7,
                    inner_index: 8,
                },
                ..IdentifierExpr::default()
            }),
        );
        let mut chain = Expr::new(
            Loc::default(),
            ExprData::Dot(crate::internal::js_ast::DotExpr {
                target: Expr::new(
                    Loc::default(),
                    ExprData::Dot(crate::internal::js_ast::DotExpr {
                        target: identifier.clone(),
                        name: "first".into(),
                        ..crate::internal::js_ast::DotExpr::default()
                    }),
                ),
                name: "second".into(),
                ..crate::internal::js_ast::DotExpr::default()
            }),
        );
        assert!(try_to_insert_optional_chain(&identifier, &mut chain));
        let Some(ExprData::Dot(outer)) = chain.data.as_deref() else {
            panic!("expected outer property access");
        };
        assert_eq!(outer.optional_chain, OptionalChain::Continue);
        assert!(matches!(
            outer.target.data.as_deref(),
            Some(ExprData::Dot(inner)) if inner.optional_chain == OptionalChain::Start
        ));
    }

    #[test]
    fn folds_numeric_string_and_shift_operators() {
        let folded = fold_binary_operator(
            Loc::default(),
            &BinaryExpr {
                left: number(1.0),
                right: number(2.0),
                op: OpCode::BinaryAdd,
            },
        )
        .expect("addition should fold");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::Number(3.0))
        ));

        let string = |value: &str| {
            Expr::new(
                Loc::default(),
                ExprData::String(StringExpr {
                    value: value.encode_utf16().collect(),
                    ..StringExpr::default()
                }),
            )
        };
        let folded = fold_binary_operator(
            Loc::default(),
            &BinaryExpr {
                left: string("a"),
                right: string("b"),
                op: OpCode::BinaryAdd,
            },
        )
        .expect("string addition should fold");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::String(value))
                if value.value == "ab".encode_utf16().collect::<Vec<_>>()
        ));

        let folded = fold_binary_operator(
            Loc::default(),
            &BinaryExpr {
                left: number(-1.0),
                right: number(0.0),
                op: OpCode::BinaryUnsignedShiftRight,
            },
        )
        .expect("unsigned shift should fold");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::Number(value))
                if value.to_bits() == f64::from(u32::MAX).to_bits()
        ));
    }

    #[test]
    fn selects_binary_folds_that_shrink_minified_output() {
        assert!(should_fold_binary_operator_when_minifying(&BinaryExpr {
            left: number(1.0),
            right: number(2.0),
            op: OpCode::BinaryAdd,
        }));
        assert!(!should_fold_binary_operator_when_minifying(&BinaryExpr {
            left: number(1e20),
            right: number(2.0),
            op: OpCode::BinaryAdd,
        }));
        assert!(should_fold_binary_operator_when_minifying(&BinaryExpr {
            left: number(1.0),
            right: number(0.0),
            op: OpCode::BinaryDivide,
        }));
    }

    #[test]
    fn tracks_truthiness_nullishness_and_side_effects() {
        assert_eq!(
            to_boolean_with_side_effects(number(0.0).data.as_deref()),
            Some((false, SideEffects::NoSideEffects))
        );
        assert_eq!(
            to_boolean_with_side_effects(
                Expr::new(Loc::default(), ExprData::Array(ArrayExpr::default()))
                    .data
                    .as_deref()
            ),
            Some((true, SideEffects::CouldHaveSideEffects))
        );
        assert_eq!(
            to_null_or_undefined_with_side_effects(
                Expr::new(Loc::default(), ExprData::Null).data.as_deref()
            ),
            Some((true, SideEffects::NoSideEffects))
        );
        assert_eq!(
            to_null_or_undefined_with_side_effects(
                Expr::new(
                    Loc::default(),
                    ExprData::Unary(UnaryExpr {
                        value: number(1.0),
                        op: OpCode::UnaryVoid,
                        ..UnaryExpr::default()
                    })
                )
                .data
                .as_deref()
            ),
            Some((true, SideEffects::CouldHaveSideEffects))
        );
    }

    #[test]
    fn folds_short_circuit_operators_without_dropping_effects() {
        let right = number(2.0);
        let folded = fold_binary_operator(
            Loc::default(),
            &BinaryExpr {
                left: Expr::new(Loc::default(), ExprData::Boolean(false)),
                right: right.clone(),
                op: OpCode::BinaryLogicalAnd,
            },
        )
        .expect("false && value should fold");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::Boolean(false))
        ));

        let folded = fold_binary_operator(
            Loc::default(),
            &BinaryExpr {
                left: Expr::new(Loc::default(), ExprData::Null),
                right: right.clone(),
                op: OpCode::BinaryNullishCoalescing,
            },
        )
        .expect("null ?? value should fold");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::Number(2.0))
        ));

        assert!(
            fold_binary_operator(
                Loc::default(),
                &BinaryExpr {
                    left: Expr::new(
                        Loc::default(),
                        ExprData::Unary(UnaryExpr {
                            value: number(1.0),
                            op: OpCode::UnaryVoid,
                            ..UnaryExpr::default()
                        })
                    ),
                    right,
                    op: OpCode::BinaryNullishCoalescing,
                },
            )
            .is_none()
        );
    }

    #[test]
    fn folds_string_addition_and_literal_arrays() {
        let string = |value: &str| {
            Expr::new(
                Loc::default(),
                ExprData::String(StringExpr {
                    value: value.encode_utf16().collect(),
                    ..StringExpr::default()
                }),
            )
        };
        let array = Expr::new(
            Loc::default(),
            ExprData::Array(ArrayExpr {
                items: vec![
                    number(1.0),
                    Expr::new(Loc::default(), ExprData::Null),
                    Expr::new(Loc::default(), ExprData::Boolean(true)),
                ],
                ..ArrayExpr::default()
            }),
        );
        let folded = fold_string_addition(array, string("!"), StringAdditionKind::Normal)
            .expect("literal array should stringify");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::String(value))
                if value.value == "1,,true!".encode_utf16().collect::<Vec<_>>()
        ));

        let folded = fold_string_addition(number(1.0), string("x"), StringAdditionKind::Normal)
            .expect("safe number should stringify");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::String(value))
                if value.value == "1x".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(
            fold_string_addition(number(1.0), string("x"), StringAdditionKind::WithNestedLeft,)
                .is_none()
        );
    }

    #[test]
    fn merges_untagged_templates_and_inlines_primitive_substitutions() {
        let string = |value: &str| {
            Expr::new(
                Loc::default(),
                ExprData::String(StringExpr {
                    value: value.encode_utf16().collect(),
                    ..StringExpr::default()
                }),
            )
        };
        let template = Expr::new(
            Loc::default(),
            ExprData::Template(TemplateExpr {
                head_cooked: "b".encode_utf16().collect(),
                parts: vec![TemplatePart {
                    value: Expr::new(
                        Loc::default(),
                        ExprData::Identifier(IdentifierExpr::default()),
                    ),
                    tail_cooked: "c".encode_utf16().collect(),
                    ..TemplatePart::default()
                }],
                ..TemplateExpr::default()
            }),
        );
        let folded = fold_string_addition(string("a"), template, StringAdditionKind::Normal)
            .expect("string and template should merge");
        assert!(matches!(
            folded.data.as_deref(),
            Some(ExprData::Template(value))
                if value.head_cooked == "ab".encode_utf16().collect::<Vec<_>>()
                    && value.parts.len() == 1
        ));

        let inlined = inline_primitives_into_template(
            Loc::default(),
            &TemplateExpr {
                head_cooked: "a".encode_utf16().collect(),
                parts: vec![TemplatePart {
                    value: string("b"),
                    tail_cooked: "c".encode_utf16().collect(),
                    ..TemplatePart::default()
                }],
                ..TemplateExpr::default()
            },
        );
        assert!(matches!(
            inlined.data.as_deref(),
            Some(ExprData::String(value))
                if value.value == "abc".encode_utf16().collect::<Vec<_>>()
                    && value.prefer_template
        ));
    }

    #[test]
    fn simplifies_expressions_in_boolean_contexts() {
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference: Ref {
                    source_index: 1,
                    inner_index: 9,
                },
                ..IdentifierExpr::default()
            }),
        );
        let double_not = Expr::new(
            Loc::default(),
            ExprData::Unary(UnaryExpr {
                value: Expr::new(
                    Loc::default(),
                    ExprData::Unary(UnaryExpr {
                        value: identifier.clone(),
                        op: OpCode::UnaryNot,
                        ..UnaryExpr::default()
                    }),
                ),
                op: OpCode::UnaryNot,
                ..UnaryExpr::default()
            }),
        );
        assert!(values_look_the_same(
            simplify_boolean_expr(&double_not, &|_| false)
                .data
                .as_deref(),
            identifier.data.as_deref()
        ));

        let conditional = Expr::new(
            Loc::default(),
            ExprData::If(IfExpr {
                test: identifier.clone(),
                yes: Expr::new(Loc::default(), ExprData::Boolean(true)),
                no: number(0.0),
            }),
        );
        assert!(matches!(
            simplify_boolean_expr(&conditional, &|_| false)
                .data
                .as_deref(),
            Some(ExprData::Binary(BinaryExpr {
                op: OpCode::BinaryLogicalOr,
                ..
            }))
        ));

        let object = Expr::new(Loc::default(), ExprData::Object(ObjectExpr::default()));
        assert!(matches!(
            simplify_boolean_expr(&object, &|_| true).data.as_deref(),
            Some(ExprData::Boolean(true))
        ));
    }

    #[test]
    fn detects_paired_null_and_undefined_checks() {
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference: Ref {
                    source_index: 2,
                    inner_index: 3,
                },
                ..IdentifierExpr::default()
            }),
        );
        let comparison = |right: Expr| {
            Expr::new(
                Loc::default(),
                ExprData::Binary(BinaryExpr {
                    left: identifier.clone(),
                    right,
                    op: OpCode::BinaryStrictEqual,
                }),
            )
        };
        assert!(
            is_binary_null_and_undefined(
                &comparison(Expr::new(Loc::default(), ExprData::Null)),
                &comparison(Expr::new(Loc::default(), ExprData::Undefined)),
                OpCode::BinaryStrictEqual,
            )
            .is_some()
        );
    }

    #[test]
    fn visits_identifier_bindings_recursively() {
        let mut binding = Binding {
            data: Some(Box::new(BindingData::Object(ObjectBindingPattern {
                properties: vec![PropertyBinding {
                    value: Binding {
                        loc: Loc { start: 42 },
                        data: Some(Box::new(BindingData::Identifier(IdentifierBinding {
                            reference: Ref {
                                source_index: 1,
                                inner_index: 2,
                            },
                        }))),
                    },
                    ..PropertyBinding::default()
                }],
                ..ObjectBindingPattern::default()
            }))),
            ..Binding::default()
        };
        let mut visited = Vec::new();
        for_each_identifier_binding(&mut binding, &mut |loc, identifier| {
            visited.push(loc.start);
            identifier.reference.inner_index += 1;
        });
        assert_eq!(visited, vec![42]);
        assert!(matches!(
            binding.data.as_deref(),
            Some(BindingData::Object(value))
                if matches!(
                    value.properties[0].value.data.as_deref(),
                    Some(BindingData::Identifier(identifier))
                        if identifier.reference.inner_index == 3
                )
        ));
    }

    #[test]
    fn determines_when_expressions_are_safe_to_tree_shake() {
        let reference = Ref {
            source_index: 4,
            inner_index: 5,
        };
        let context = make_helper_context(move |candidate| candidate == reference);
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference,
                ..IdentifierExpr::default()
            }),
        );
        assert!(!context.expr_can_be_removed_if_unused(&identifier));
        assert!(context.expr_can_be_removed_if_unused(&number(1.0)));

        let typeof_identifier = Expr::new(
            Loc::default(),
            ExprData::Unary(UnaryExpr {
                value: identifier.clone(),
                op: OpCode::UnaryTypeof,
                was_originally_typeof_identifier: true,
                ..UnaryExpr::default()
            }),
        );
        assert!(context.expr_can_be_removed_if_unused(&typeof_identifier));

        let pure_call = Expr::new(
            Loc::default(),
            ExprData::Call(CallExpr {
                target: identifier.clone(),
                args: vec![number(1.0)],
                can_be_unwrapped_if_unused: true,
                ..CallExpr::default()
            }),
        );
        assert!(context.expr_can_be_removed_if_unused(&pure_call));

        let object_spread = Expr::new(
            Loc::default(),
            ExprData::Object(ObjectExpr {
                properties: vec![Property {
                    kind: PropertyKind::Spread,
                    value_or_nil: number(1.0),
                    ..Property::default()
                }],
                ..ObjectExpr::default()
            }),
        );
        assert!(!context.expr_can_be_removed_if_unused(&object_spread));
    }

    #[test]
    fn recognizes_typeof_guards_for_unbound_identifiers() {
        let reference = Ref {
            source_index: 8,
            inner_index: 9,
        };
        let context = make_helper_context(move |candidate| candidate == reference);
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference,
                ..IdentifierExpr::default()
            }),
        );
        let guard = Expr::new(
            Loc::default(),
            ExprData::Binary(BinaryExpr {
                left: Expr::new(
                    Loc::default(),
                    ExprData::Unary(UnaryExpr {
                        value: identifier.clone(),
                        op: OpCode::UnaryTypeof,
                        was_originally_typeof_identifier: true,
                        ..UnaryExpr::default()
                    }),
                ),
                right: Expr::new(
                    Loc::default(),
                    ExprData::String(StringExpr {
                        value: "undefined".encode_utf16().collect(),
                        ..StringExpr::default()
                    }),
                ),
                op: OpCode::BinaryStrictNotEqual,
            }),
        );
        let guarded = Expr::new(
            Loc::default(),
            ExprData::Binary(BinaryExpr {
                left: guard,
                right: identifier,
                op: OpCode::BinaryLogicalAnd,
            }),
        );
        assert!(context.expr_can_be_removed_if_unused(&guarded));
    }

    #[test]
    fn analyzes_classes_and_statements_for_tree_shaking() {
        let context = make_helper_context(|_| false);
        let mut class = Class {
            use_define_for_class_fields: true,
            properties: vec![Property {
                kind: PropertyKind::Field,
                flags: PropertyFlags::IS_STATIC,
                value_or_nil: number(1.0),
                ..Property::default()
            }],
            ..Class::default()
        };
        assert!(context.class_can_be_removed_if_unused(&class));
        class.use_define_for_class_fields = false;
        assert!(!context.class_can_be_removed_if_unused(&class));

        let return_statement = Stmt::new(
            Loc::default(),
            StmtData::Return(ReturnStmt {
                value_or_nil: number(1.0),
            }),
        );
        assert!(!context.stmts_can_be_removed_if_unused(
            std::slice::from_ref(&return_statement),
            StmtsCanBeRemovedIfUnusedFlags::NONE,
        ));
        assert!(context.stmts_can_be_removed_if_unused(
            std::slice::from_ref(&return_statement),
            StmtsCanBeRemovedIfUnusedFlags::RETURN_CAN_BE_REMOVED_IF_UNUSED,
        ));

        let using_null = Stmt::new(
            Loc::default(),
            StmtData::Local(LocalStmt {
                kind: LocalKind::Using,
                declarations: vec![Decl {
                    binding: Binding {
                        data: Some(Box::new(BindingData::Identifier(
                            IdentifierBinding::default(),
                        ))),
                        ..Binding::default()
                    },
                    value_or_nil: Expr::new(Loc::default(), ExprData::Null),
                }],
                ..LocalStmt::default()
            }),
        );
        assert!(
            context.stmts_can_be_removed_if_unused(
                &[using_null],
                StmtsCanBeRemovedIfUnusedFlags::NONE,
            )
        );

        let generated_effect = Stmt::new(
            Loc::default(),
            StmtData::Expr(ExprStmt {
                value: Expr::new(Loc::default(), ExprData::Call(CallExpr::default())),
                is_from_class_or_fn_that_can_be_removed_if_unused: true,
            }),
        );
        assert!(context.stmts_can_be_removed_if_unused(
            &[generated_effect],
            StmtsCanBeRemovedIfUnusedFlags::NONE,
        ));
    }

    #[test]
    fn simplifies_unused_literals_arrays_and_conditionals() {
        let context = make_helper_context(|_| false);
        assert!(
            context
                .simplify_unused_expr(&number(1.0), JsFeature::NONE)
                .data
                .is_none()
        );

        let call = Expr::new(Loc::default(), ExprData::Call(CallExpr::default()));
        let array = Expr::new(
            Loc::default(),
            ExprData::Array(ArrayExpr {
                items: vec![number(1.0), call.clone()],
                ..ArrayExpr::default()
            }),
        );
        assert!(matches!(
            context
                .simplify_unused_expr(&array, JsFeature::NONE)
                .data
                .as_deref(),
            Some(ExprData::Call(_))
        ));

        let conditional = Expr::new(
            Loc::default(),
            ExprData::If(IfExpr {
                test: Expr::new(
                    Loc::default(),
                    ExprData::Identifier(IdentifierExpr::default()),
                ),
                yes: number(1.0),
                no: call,
            }),
        );
        assert!(matches!(
            context
                .simplify_unused_expr(&conditional, JsFeature::NONE)
                .data
                .as_deref(),
            Some(ExprData::Binary(BinaryExpr {
                op: OpCode::BinaryLogicalOr,
                ..
            }))
        ));
    }

    #[test]
    fn preserves_spread_and_computed_key_evaluation_when_unused() {
        let context = make_helper_context(|_| false);
        let spread = Expr::new(
            Loc::default(),
            ExprData::Spread(SpreadExpr {
                value: Expr::new(
                    Loc::default(),
                    ExprData::Identifier(IdentifierExpr::default()),
                ),
            }),
        );
        let array = Expr::new(
            Loc::default(),
            ExprData::Array(ArrayExpr {
                items: vec![number(1.0), spread],
                ..ArrayExpr::default()
            }),
        );
        assert!(matches!(
            context
                .simplify_unused_expr(&array, JsFeature::NONE)
                .data
                .as_deref(),
            Some(ExprData::Array(value)) if value.items.len() == 1
        ));

        let object = Expr::new(
            Loc::default(),
            ExprData::Object(ObjectExpr {
                properties: vec![Property {
                    key: Expr::new(
                        Loc::default(),
                        ExprData::Identifier(IdentifierExpr::default()),
                    ),
                    value_or_nil: number(1.0),
                    flags: PropertyFlags::IS_COMPUTED,
                    ..Property::default()
                }],
                ..ObjectExpr::default()
            }),
        );
        assert!(matches!(
            context
                .simplify_unused_expr(&object, JsFeature::NONE)
                .data
                .as_deref(),
            Some(ExprData::Binary(BinaryExpr {
                op: OpCode::BinaryAdd,
                ..
            }))
        ));
    }

    #[test]
    fn simplifies_null_checks_into_optional_chains() {
        let reference = Ref {
            source_index: 11,
            inner_index: 12,
        };
        let context = make_helper_context(|_| false);
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference,
                ..IdentifierExpr::default()
            }),
        );
        let expression = Expr::new(
            Loc::default(),
            ExprData::Binary(BinaryExpr {
                left: Expr::new(
                    Loc::default(),
                    ExprData::Binary(BinaryExpr {
                        left: identifier.clone(),
                        right: Expr::new(Loc::default(), ExprData::Null),
                        op: OpCode::BinaryLooseNotEqual,
                    }),
                ),
                right: Expr::new(
                    Loc::default(),
                    ExprData::Dot(crate::internal::js_ast::DotExpr {
                        target: identifier,
                        name: "value".into(),
                        ..crate::internal::js_ast::DotExpr::default()
                    }),
                ),
                op: OpCode::BinaryLogicalAnd,
            }),
        );
        assert!(matches!(
            context
                .simplify_unused_expr(&expression, JsFeature::NONE)
                .data
                .as_deref(),
            Some(ExprData::Dot(value))
                if value.optional_chain == OptionalChain::Start
        ));
    }

    #[test]
    fn unwraps_pure_calls_and_empty_iifes_when_unused() {
        let context = make_helper_context(|_| false);
        let effect = Expr::new(Loc::default(), ExprData::Call(CallExpr::default()));
        let pure = Expr::new(
            Loc::default(),
            ExprData::Call(CallExpr {
                args: vec![number(1.0), effect],
                can_be_unwrapped_if_unused: true,
                ..CallExpr::default()
            }),
        );
        assert!(matches!(
            context
                .simplify_unused_expr(&pure, JsFeature::NONE)
                .data
                .as_deref(),
            Some(ExprData::Call(_))
        ));

        let empty_iife = Expr::new(
            Loc::default(),
            ExprData::Call(CallExpr {
                target: Expr::new(
                    Loc::default(),
                    ExprData::Function(FunctionExpr {
                        function: Function::default(),
                        ..FunctionExpr::default()
                    }),
                ),
                ..CallExpr::default()
            }),
        );
        assert!(
            context
                .simplify_unused_expr(&empty_iife, JsFeature::NONE)
                .data
                .is_none()
        );
    }
}
