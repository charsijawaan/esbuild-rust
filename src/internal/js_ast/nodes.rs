use crate::internal::logger::platform_independent_path_dir_base_ext;
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

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum LocalKind {
    #[default]
    Var,
    Let,
    Const,
    Using,
    AwaitUsing,
}

impl LocalKind {
    #[must_use]
    pub fn is_using(self) -> bool {
        self >= Self::Using
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum ScopeKind {
    #[default]
    Block,
    With,
    Label,
    ClassName,
    ClassBody,
    CatchBinding,
    Entry,
    FunctionArgs,
    FunctionBody,
    ClassStaticInit,
}

impl ScopeKind {
    #[must_use]
    pub fn stops_hoisting(self) -> bool {
        self >= Self::Entry
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum StrictModeKind {
    #[default]
    Sloppy,
    ExplicitStrict,
    ImplicitStrictClass,
    ImplicitStrictEsm,
    ImplicitStrictTsAlwaysStrict,
    ImplicitStrictJsxAutomaticRuntime,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum ExportsKind {
    #[default]
    None,
    CommonJs,
    Esm,
    EsmWithDynamicFallback,
}

impl ExportsKind {
    #[must_use]
    pub const fn is_dynamic(self) -> bool {
        matches!(self, Self::CommonJs | Self::EsmWithDynamicFallback)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum ModuleType {
    #[default]
    Unknown,
    CommonJsCjs,
    CommonJsCts,
    CommonJsPackageJson,
    EsmMjs,
    EsmMts,
    EsmPackageJson,
}

impl ModuleType {
    #[must_use]
    pub fn is_common_js(self) -> bool {
        (Self::CommonJsCjs..=Self::CommonJsPackageJson).contains(&self)
    }

    #[must_use]
    pub fn is_esm(self) -> bool {
        (Self::EsmMjs..=Self::EsmPackageJson).contains(&self)
    }
}

pub const NS_EXPORT_PART_INDEX: u32 = 0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum ConstValueKind {
    #[default]
    None,
    Null,
    Undefined,
    True,
    False,
    Number,
    String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConstValue {
    pub number: f64,
    pub string: Vec<u16>,
    pub kind: ConstValueKind,
}

#[must_use]
pub fn generate_non_unique_name_from_path(path: &str) -> String {
    let (directory, mut base, _) = platform_independent_path_dir_base_ext(path);
    if base == "index" {
        let (_, directory_base, _) = platform_independent_path_dir_base_ext(&directory);
        if !directory_base.is_empty() {
            base = directory_base;
        }
    }
    ensure_valid_identifier(&base)
}

#[must_use]
pub fn ensure_valid_identifier(base: &str) -> String {
    let mut bytes = Vec::new();
    let mut needs_gap = false;
    for character in base.chars() {
        let is_letter = character.is_ascii_alphabetic();
        let is_non_initial_digit = !bytes.is_empty() && character.is_ascii_digit();
        if is_letter || is_non_initial_digit {
            if needs_gap {
                bytes.push(b'_');
                needs_gap = false;
            }
            let mut encoded = [0; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        } else if !bytes.is_empty() {
            needs_gap = true;
        }
    }
    if bytes.is_empty() {
        "_".into()
    } else {
        bytes.into_iter().map(char::from).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnnotationFlags, AssignTarget, ExportsKind, LocalKind, ModuleType, OP_TABLE, OpCode,
        Precedence, PropertyFlags, PropertyKind, ScopeKind, ensure_valid_identifier,
        generate_non_unique_name_from_path,
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

    #[test]
    fn scope_module_and_export_classification_matches_boundaries() {
        assert!(!LocalKind::Const.is_using());
        assert!(LocalKind::Using.is_using());
        assert!(!ScopeKind::CatchBinding.stops_hoisting());
        assert!(ScopeKind::Entry.stops_hoisting());
        assert!(ExportsKind::CommonJs.is_dynamic());
        assert!(!ExportsKind::Esm.is_dynamic());
        assert!(ModuleType::CommonJsCts.is_common_js());
        assert!(!ModuleType::EsmMts.is_common_js());
        assert!(ModuleType::EsmPackageJson.is_esm());
    }

    #[test]
    fn generated_names_follow_upstream_path_and_ascii_rules() {
        assert_eq!(ensure_valid_identifier("hello-world"), "hello_world");
        assert_eq!(ensure_valid_identifier("123"), "_");
        assert_eq!(ensure_valid_identifier("x--y"), "x_y");
        assert_eq!(ensure_valid_identifier("πx"), "x");
        assert_eq!(
            generate_non_unique_name_from_path("/packages/react/index.js"),
            "react"
        );
        assert_eq!(
            generate_non_unique_name_from_path("C:\\src\\hello-world.ts"),
            "hello_world"
        );
    }
}
