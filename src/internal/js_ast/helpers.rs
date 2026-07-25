use super::{
    AnnotationFlags, ArrayExpr, AssignTarget, BinaryExpr, Binding, BindingData, Expr, ExprData,
    ExprStmt, IdentifierExpr, ObjectExpr, OpCode, OptionalChain, Property, PropertyFlags,
    PropertyKind, SpreadExpr, Stmt, StmtData, UnaryExpr,
};
use crate::internal::ast::Ref;
use crate::internal::logger::Loc;

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

#[cfg(test)]
mod tests {
    use super::{
        PrimitiveType, assign, can_change_strict_to_loose, convert_binding_to_expr,
        is_optional_chain, join_all_with_comma, known_primitive_type, maybe_simplify_not, not,
        typeof_without_side_effects,
    };
    use crate::internal::ast::Ref;
    use crate::internal::js_ast::{
        ArrayBinding, ArrayBindingPattern, BinaryExpr, Binding, BindingData, Expr, ExprData,
        IdentifierBinding, OpCode, OptionalChain, StringExpr,
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
}
