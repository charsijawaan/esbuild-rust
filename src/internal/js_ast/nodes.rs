use std::ops::{BitOr, BitOrAssign};

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Precedence {
    #[default]
    Lowest,
    Comma,
    Spread,
    Yield,
    Assign,
    Conditional,
    NullishCoalescing,
    LogicalOr,
    LogicalAnd,
    BitwiseOr,
    BitwiseXor,
    BitwiseAnd,
    Equals,
    Compare,
    Shift,
    Add,
    Multiply,
    Exponentiation,
    Prefix,
    Postfix,
    New,
    Call,
    Member,
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum OpCode {
    #[default]
    UnaryPositive,
    UnaryNegative,
    UnaryComplement,
    UnaryNot,
    UnaryVoid,
    UnaryTypeof,
    UnaryDelete,
    UnaryPreDecrement,
    UnaryPreIncrement,
    UnaryPostDecrement,
    UnaryPostIncrement,
    BinaryAdd,
    BinarySubtract,
    BinaryMultiply,
    BinaryDivide,
    BinaryRemainder,
    BinaryPower,
    BinaryLessThan,
    BinaryLessThanOrEqual,
    BinaryGreaterThan,
    BinaryGreaterThanOrEqual,
    BinaryIn,
    BinaryInstanceof,
    BinaryShiftLeft,
    BinaryShiftRight,
    BinaryUnsignedShiftRight,
    BinaryLooseEqual,
    BinaryLooseNotEqual,
    BinaryStrictEqual,
    BinaryStrictNotEqual,
    BinaryNullishCoalescing,
    BinaryLogicalOr,
    BinaryLogicalAnd,
    BinaryBitwiseOr,
    BinaryBitwiseAnd,
    BinaryBitwiseXor,
    BinaryComma,
    BinaryAssign,
    BinaryAddAssign,
    BinarySubtractAssign,
    BinaryMultiplyAssign,
    BinaryDivideAssign,
    BinaryRemainderAssign,
    BinaryPowerAssign,
    BinaryShiftLeftAssign,
    BinaryShiftRightAssign,
    BinaryUnsignedShiftRightAssign,
    BinaryBitwiseOrAssign,
    BinaryBitwiseAndAssign,
    BinaryBitwiseXorAssign,
    BinaryNullishCoalescingAssign,
    BinaryLogicalOrAssign,
    BinaryLogicalAndAssign,
}

impl OpCode {
    #[must_use]
    pub fn is_prefix(self) -> bool {
        self < Self::UnaryPostDecrement
    }

    #[must_use]
    pub fn unary_assign_target(self) -> AssignTarget {
        if (Self::UnaryPreDecrement..=Self::UnaryPostIncrement).contains(&self) {
            AssignTarget::Update
        } else {
            AssignTarget::None
        }
    }

    #[must_use]
    pub fn is_left_associative(self) -> bool {
        (Self::BinaryAdd..Self::BinaryComma).contains(&self) && self != Self::BinaryPower
    }

    #[must_use]
    pub fn is_right_associative(self) -> bool {
        self >= Self::BinaryAssign || self == Self::BinaryPower
    }

    #[must_use]
    pub fn binary_assign_target(self) -> AssignTarget {
        match self.cmp(&Self::BinaryAssign) {
            std::cmp::Ordering::Equal => AssignTarget::Replace,
            std::cmp::Ordering::Greater => AssignTarget::Update,
            std::cmp::Ordering::Less => AssignTarget::None,
        }
    }

    #[must_use]
    pub fn is_short_circuit(self) -> bool {
        matches!(
            self,
            Self::BinaryLogicalOr
                | Self::BinaryLogicalOrAssign
                | Self::BinaryLogicalAnd
                | Self::BinaryLogicalAndAssign
                | Self::BinaryNullishCoalescing
                | Self::BinaryNullishCoalescingAssign
        )
    }

    #[must_use]
    pub const fn table_entry(self) -> &'static OpTableEntry {
        &OP_TABLE[self as usize]
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum AssignTarget {
    #[default]
    None,
    Replace,
    Update,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpTableEntry {
    pub text: &'static str,
    pub level: Precedence,
    pub is_keyword: bool,
}

pub const OP_TABLE: &[OpTableEntry] = &[
    op("+", Precedence::Prefix, false),
    op("-", Precedence::Prefix, false),
    op("~", Precedence::Prefix, false),
    op("!", Precedence::Prefix, false),
    op("void", Precedence::Prefix, true),
    op("typeof", Precedence::Prefix, true),
    op("delete", Precedence::Prefix, true),
    op("--", Precedence::Prefix, false),
    op("++", Precedence::Prefix, false),
    op("--", Precedence::Postfix, false),
    op("++", Precedence::Postfix, false),
    op("+", Precedence::Add, false),
    op("-", Precedence::Add, false),
    op("*", Precedence::Multiply, false),
    op("/", Precedence::Multiply, false),
    op("%", Precedence::Multiply, false),
    op("**", Precedence::Exponentiation, false),
    op("<", Precedence::Compare, false),
    op("<=", Precedence::Compare, false),
    op(">", Precedence::Compare, false),
    op(">=", Precedence::Compare, false),
    op("in", Precedence::Compare, true),
    op("instanceof", Precedence::Compare, true),
    op("<<", Precedence::Shift, false),
    op(">>", Precedence::Shift, false),
    op(">>>", Precedence::Shift, false),
    op("==", Precedence::Equals, false),
    op("!=", Precedence::Equals, false),
    op("===", Precedence::Equals, false),
    op("!==", Precedence::Equals, false),
    op("??", Precedence::NullishCoalescing, false),
    op("||", Precedence::LogicalOr, false),
    op("&&", Precedence::LogicalAnd, false),
    op("|", Precedence::BitwiseOr, false),
    op("&", Precedence::BitwiseAnd, false),
    op("^", Precedence::BitwiseXor, false),
    op(",", Precedence::Comma, false),
    op("=", Precedence::Assign, false),
    op("+=", Precedence::Assign, false),
    op("-=", Precedence::Assign, false),
    op("*=", Precedence::Assign, false),
    op("/=", Precedence::Assign, false),
    op("%=", Precedence::Assign, false),
    op("**=", Precedence::Assign, false),
    op("<<=", Precedence::Assign, false),
    op(">>=", Precedence::Assign, false),
    op(">>>=", Precedence::Assign, false),
    op("|=", Precedence::Assign, false),
    op("&=", Precedence::Assign, false),
    op("^=", Precedence::Assign, false),
    op("??=", Precedence::Assign, false),
    op("||=", Precedence::Assign, false),
    op("&&=", Precedence::Assign, false),
];

const fn op(text: &'static str, level: Precedence, is_keyword: bool) -> OpTableEntry {
    OpTableEntry {
        text,
        level,
        is_keyword,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum PropertyKind {
    #[default]
    Field,
    Method,
    Getter,
    Setter,
    AutoAccessor,
    Spread,
    DeclareOrAbstract,
    ClassStaticBlock,
}

impl PropertyKind {
    #[must_use]
    pub const fn is_method_definition(self) -> bool {
        matches!(self, Self::Method | Self::Getter | Self::Setter)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PropertyFlags(u8);

impl PropertyFlags {
    pub const NONE: Self = Self(0);
    pub const IS_COMPUTED: Self = Self(1 << 0);
    pub const IS_STATIC: Self = Self(1 << 1);
    pub const WAS_SHORTHAND: Self = Self(1 << 2);
    pub const PREFER_QUOTED_KEY: Self = Self(1 << 3);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for PropertyFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl BitOrAssign for PropertyFlags {
    fn bitor_assign(&mut self, right: Self) {
        self.0 |= right.0;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum CallKind {
    #[default]
    Normal,
    DirectEval,
    TargetWasOriginallyPropertyAccess,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum OptionalChain {
    #[default]
    None,
    Start,
    Continue,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnnotationFlags(u8);

impl AnnotationFlags {
    pub const NONE: Self = Self(0);
    pub const CAN_BE_REMOVED_IF_UNUSED: Self = Self(1);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnnotationFlags, AssignTarget, OP_TABLE, OpCode, Precedence, PropertyFlags, PropertyKind,
    };

    #[test]
    fn operator_table_order_and_metadata_match_enum_order() {
        assert_eq!(OP_TABLE.len(), OpCode::BinaryLogicalAndAssign as usize + 1);
        assert_eq!(OpCode::UnaryTypeof.table_entry().text, "typeof");
        assert!(OpCode::UnaryTypeof.table_entry().is_keyword);
        assert_eq!(
            OpCode::BinaryPower.table_entry().level,
            Precedence::Exponentiation
        );
        assert_eq!(
            OpCode::BinaryUnsignedShiftRightAssign.table_entry().text,
            ">>>="
        );
    }

    #[test]
    fn operator_classification_matches_upstream_boundaries() {
        assert!(OpCode::UnaryPreIncrement.is_prefix());
        assert!(!OpCode::UnaryPostDecrement.is_prefix());
        assert_eq!(
            OpCode::UnaryPostIncrement.unary_assign_target(),
            AssignTarget::Update
        );
        assert!(OpCode::BinaryAdd.is_left_associative());
        assert!(!OpCode::BinaryPower.is_left_associative());
        assert!(OpCode::BinaryPower.is_right_associative());
        assert_eq!(
            OpCode::BinaryAssign.binary_assign_target(),
            AssignTarget::Replace
        );
        assert_eq!(
            OpCode::BinaryAddAssign.binary_assign_target(),
            AssignTarget::Update
        );
        assert!(OpCode::BinaryNullishCoalescingAssign.is_short_circuit());
    }

    #[test]
    fn property_and_annotation_flags_preserve_bit_semantics() {
        assert!(PropertyKind::Getter.is_method_definition());
        assert!(!PropertyKind::AutoAccessor.is_method_definition());
        let flags = PropertyFlags::IS_STATIC | PropertyFlags::PREFER_QUOTED_KEY;
        assert!(flags.contains(PropertyFlags::IS_STATIC));
        assert!(!flags.contains(PropertyFlags::WAS_SHORTHAND));
        assert!(
            AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED
                .contains(AnnotationFlags::CAN_BE_REMOVED_IF_UNUSED)
        );
    }
}
