use super::{
    AnnotationFlags, ArrayExpr, AssignTarget, BinaryExpr, Binding, BindingData, Expr, ExprData,
    ExprStmt, IdentifierExpr, ObjectExpr, OpCode, OptionalChain, Property, PropertyFlags,
    PropertyKind, SpreadExpr, Stmt, StmtData, UnaryExpr,
};
use crate::internal::ast::Ref;
use crate::internal::helpers::utf16_equals_wtf8;
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
        PrimitiveType, assign, can_change_strict_to_loose, check_equality_big_int,
        convert_binding_to_expr, inline_spreads_of_array_literals, is_optional_chain,
        join_all_with_comma, known_primitive_type, mangle_object_spread, maybe_simplify_not, not,
        string_compare_ucs2, string_to_equivalent_number_value, to_int32,
        to_number_without_side_effects, to_string_without_side_effects, to_uint32,
        try_to_string_on_number_safely, typeof_without_side_effects,
    };
    use crate::internal::ast::Ref;
    use crate::internal::js_ast::{
        ArrayBinding, ArrayBindingPattern, ArrayExpr, BinaryExpr, Binding, BindingData, Expr,
        ExprData, IdentifierBinding, ObjectExpr, OpCode, OptionalChain, Property, PropertyKind,
        SpreadExpr, StringExpr,
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
}
