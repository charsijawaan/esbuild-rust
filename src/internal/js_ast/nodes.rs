use crate::internal::ast::{ImportPhase, LocRef, Ref};
use crate::internal::logger::{Loc, Range, platform_independent_path_dir_base_ext};
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

#[derive(Clone, Debug, Default)]
pub struct Decorator {
    pub value: Expr,
    pub at_loc: Loc,
    pub omit_newline_after: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ClassStaticBlock {
    pub block: BlockStmt,
    pub loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct Property {
    pub class_static_block: Option<Box<ClassStaticBlock>>,
    pub key: Expr,
    pub value_or_nil: Expr,
    pub initializer_or_nil: Expr,
    pub decorators: Vec<Decorator>,
    pub loc: Loc,
    pub close_bracket_loc: Loc,
    pub kind: PropertyKind,
    pub flags: PropertyFlags,
}

#[derive(Clone, Debug, Default)]
pub struct PropertyBinding {
    pub key: Expr,
    pub value: Binding,
    pub default_value_or_nil: Expr,
    pub loc: Loc,
    pub close_bracket_loc: Loc,
    pub is_computed: bool,
    pub is_spread: bool,
    pub prefer_quoted_key: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Arg {
    pub binding: Binding,
    pub default_or_nil: Expr,
    pub decorators: Vec<Decorator>,
    pub is_typescript_ctor_field: bool,
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Function {
    pub name: Option<LocRef>,
    pub args: Vec<Arg>,
    pub body: FunctionBody,
    pub arguments_ref: Ref,
    pub open_paren_loc: Loc,
    pub is_async: bool,
    pub is_generator: bool,
    pub has_rest_arg: bool,
    pub has_if_scope: bool,
    pub has_no_side_effects_comment: bool,
    pub is_unique_formal_parameters: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FunctionBody {
    pub block: BlockStmt,
    pub loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct Class {
    pub decorators: Vec<Decorator>,
    pub name: Option<LocRef>,
    pub extends_or_nil: Expr,
    pub properties: Vec<Property>,
    pub class_keyword: Range,
    pub body_loc: Loc,
    pub close_brace_loc: Loc,
    pub should_lower_standard_decorators: bool,
    pub use_define_for_class_fields: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ArrayBinding {
    pub binding: Binding,
    pub default_value_or_nil: Expr,
    pub loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct Binding {
    pub data: Option<Box<BindingData>>,
    pub loc: Loc,
}

#[derive(Clone, Debug)]
pub enum BindingData {
    Missing,
    Identifier(IdentifierBinding),
    Array(ArrayBindingPattern),
    Object(ObjectBindingPattern),
}

#[derive(Clone, Debug, Default)]
pub struct IdentifierBinding {
    pub reference: Ref,
}

#[derive(Clone, Debug, Default)]
pub struct ArrayBindingPattern {
    pub items: Vec<ArrayBinding>,
    pub close_bracket_loc: Loc,
    pub has_spread: bool,
    pub is_single_line: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ObjectBindingPattern {
    pub properties: Vec<PropertyBinding>,
    pub close_brace_loc: Loc,
    pub is_single_line: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Expr {
    pub data: Option<Box<ExprData>>,
    pub loc: Loc,
}

impl Expr {
    #[must_use]
    pub fn new(loc: Loc, data: ExprData) -> Self {
        Self {
            data: Some(Box::new(data)),
            loc,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ExprData {
    Array(ArrayExpr),
    Unary(UnaryExpr),
    Binary(BinaryExpr),
    Boolean(bool),
    Super,
    Null,
    Undefined,
    This,
    New(NewExpr),
    NewTarget(NewTargetExpr),
    ImportMeta(ImportMetaExpr),
    Call(CallExpr),
    Dot(DotExpr),
    Index(IndexExpr),
    Arrow(ArrowExpr),
    Function(FunctionExpr),
    Class(ClassExpr),
    Identifier(IdentifierExpr),
    ImportIdentifier(ImportIdentifierExpr),
    PrivateIdentifier(PrivateIdentifierExpr),
    NameOfSymbol(NameOfSymbolExpr),
    JsxElement(JsxElementExpr),
    JsxText(JsxTextExpr),
    Missing,
    Number(f64),
    BigInt(String),
    Object(ObjectExpr),
    Spread(SpreadExpr),
    String(StringExpr),
    Template(TemplateExpr),
    RegExp(String),
    InlinedEnum(InlinedEnumExpr),
    Annotation(AnnotationExpr),
    Await(AwaitExpr),
    Yield(YieldExpr),
    If(IfExpr),
    RequireString(RequireStringExpr),
    RequireResolveString(RequireResolveStringExpr),
    ImportString(ImportStringExpr),
    ImportCall(ImportCallExpr),
}

#[derive(Clone, Debug, Default)]
pub struct ArrayExpr {
    pub items: Vec<Expr>,
    pub comma_after_spread: Loc,
    pub close_bracket_loc: Loc,
    pub is_single_line: bool,
    pub is_parenthesized: bool,
}

#[derive(Clone, Debug, Default)]
pub struct UnaryExpr {
    pub value: Expr,
    pub op: OpCode,
    pub was_originally_typeof_identifier: bool,
    pub was_originally_delete_of_identifier_or_property_access: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BinaryExpr {
    pub left: Expr,
    pub right: Expr,
    pub op: OpCode,
}

#[derive(Clone, Debug, Default)]
pub struct NewTargetExpr {
    pub range: Range,
}

#[derive(Clone, Debug, Default)]
pub struct ImportMetaExpr {
    pub range_len: i32,
}

#[derive(Clone, Debug, Default)]
pub struct NewExpr {
    pub target: Expr,
    pub args: Vec<Expr>,
    pub close_paren_loc: Loc,
    pub is_multi_line: bool,
    pub can_be_unwrapped_if_unused: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CallExpr {
    pub target: Expr,
    pub args: Vec<Expr>,
    pub close_paren_loc: Loc,
    pub optional_chain: OptionalChain,
    pub kind: CallKind,
    pub is_multi_line: bool,
    pub can_be_unwrapped_if_unused: bool,
}

impl CallExpr {
    #[must_use]
    pub fn has_same_flags_as(&self, other: &Self) -> bool {
        self.optional_chain == other.optional_chain
            && self.kind == other.kind
            && self.can_be_unwrapped_if_unused == other.can_be_unwrapped_if_unused
    }
}

#[derive(Clone, Debug, Default)]
pub struct DotExpr {
    pub target: Expr,
    pub name: String,
    pub name_loc: Loc,
    pub optional_chain: OptionalChain,
    pub can_be_removed_if_unused: bool,
    pub call_can_be_unwrapped_if_unused: bool,
    pub is_symbol_instance: bool,
}

impl DotExpr {
    #[must_use]
    pub fn has_same_flags_as(&self, other: &Self) -> bool {
        self.optional_chain == other.optional_chain
            && self.can_be_removed_if_unused == other.can_be_removed_if_unused
            && self.call_can_be_unwrapped_if_unused == other.call_can_be_unwrapped_if_unused
            && self.is_symbol_instance == other.is_symbol_instance
    }
}

#[derive(Clone, Debug, Default)]
pub struct IndexExpr {
    pub target: Expr,
    pub index: Expr,
    pub close_bracket_loc: Loc,
    pub optional_chain: OptionalChain,
    pub can_be_removed_if_unused: bool,
    pub call_can_be_unwrapped_if_unused: bool,
    pub is_symbol_instance: bool,
}

impl IndexExpr {
    #[must_use]
    pub fn has_same_flags_as(&self, other: &Self) -> bool {
        self.optional_chain == other.optional_chain
            && self.can_be_removed_if_unused == other.can_be_removed_if_unused
            && self.call_can_be_unwrapped_if_unused == other.call_can_be_unwrapped_if_unused
            && self.is_symbol_instance == other.is_symbol_instance
    }
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ArrowExpr {
    pub args: Vec<Arg>,
    pub body: FunctionBody,
    pub is_async: bool,
    pub has_rest_arg: bool,
    pub prefer_expr: bool,
    pub is_parenthesized: bool,
    pub has_no_side_effects_comment: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FunctionExpr {
    pub function: Function,
    pub is_parenthesized: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ClassExpr {
    pub class: Class,
}

#[derive(Clone, Debug, Default)]
pub struct IdentifierExpr {
    pub reference: Ref,
    pub must_keep_due_to_with_stmt: bool,
    pub can_be_removed_if_unused: bool,
    pub call_can_be_unwrapped_if_unused: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ImportIdentifierExpr {
    pub reference: Ref,
    pub prefer_quoted_key: bool,
    pub was_originally_identifier: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PrivateIdentifierExpr {
    pub reference: Ref,
}

#[derive(Clone, Debug, Default)]
pub struct NameOfSymbolExpr {
    pub reference: Ref,
    pub has_property_key_comment: bool,
}

#[derive(Clone, Debug, Default)]
pub struct JsxElementExpr {
    pub tag_or_nil: Expr,
    pub properties: Vec<Property>,
    pub nullable_children: Vec<Expr>,
    pub close_loc: Loc,
    pub is_tag_single_line: bool,
}

#[derive(Clone, Debug, Default)]
pub struct JsxTextExpr {
    pub raw: String,
}

#[derive(Clone, Debug, Default)]
pub struct ObjectExpr {
    pub properties: Vec<Property>,
    pub comma_after_spread: Loc,
    pub close_brace_loc: Loc,
    pub is_single_line: bool,
    pub is_parenthesized: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SpreadExpr {
    pub value: Expr,
}

#[derive(Clone, Debug, Default)]
pub struct StringExpr {
    pub value: Vec<u16>,
    pub legacy_octal_loc: Loc,
    pub prefer_template: bool,
    pub has_property_key_comment: bool,
    pub contains_unique_key: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TemplatePart {
    pub value: Expr,
    pub tail_raw: String,
    pub tail_cooked: Vec<u16>,
    pub tail_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct TemplateExpr {
    pub tag_or_nil: Expr,
    pub head_raw: String,
    pub head_cooked: Vec<u16>,
    pub parts: Vec<TemplatePart>,
    pub head_loc: Loc,
    pub legacy_octal_loc: Loc,
    pub can_be_unwrapped_if_unused: bool,
    pub tag_was_originally_property_access: bool,
}

#[derive(Clone, Debug, Default)]
pub struct InlinedEnumExpr {
    pub value: Expr,
    pub comment: String,
}

#[derive(Clone, Debug, Default)]
pub struct AnnotationExpr {
    pub value: Expr,
    pub flags: AnnotationFlags,
}

#[derive(Clone, Debug, Default)]
pub struct AwaitExpr {
    pub value: Expr,
}

#[derive(Clone, Debug, Default)]
pub struct YieldExpr {
    pub value_or_nil: Expr,
    pub is_star: bool,
}

#[derive(Clone, Debug, Default)]
pub struct IfExpr {
    pub test: Expr,
    pub yes: Expr,
    pub no: Expr,
}

#[derive(Clone, Debug, Default)]
pub struct RequireStringExpr {
    pub import_record_index: u32,
    pub close_paren_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct RequireResolveStringExpr {
    pub import_record_index: u32,
    pub close_paren_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct ImportStringExpr {
    pub import_record_index: u32,
    pub close_paren_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct ImportCallExpr {
    pub expr: Expr,
    pub options_or_nil: Expr,
    pub close_paren_loc: Loc,
    pub phase: ImportPhase,
}

#[derive(Clone, Debug, Default)]
pub struct Stmt {
    pub data: Option<Box<StmtData>>,
    pub loc: Loc,
}

#[derive(Clone, Debug)]
pub enum StmtData {
    Placeholder,
}

#[derive(Clone, Debug, Default)]
pub struct BlockStmt {
    pub statements: Vec<Stmt>,
    pub close_brace_loc: Loc,
}

#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]
pub fn expr_to_const_value(expr: &Expr) -> ConstValue {
    let Some(data) = expr.data.as_deref() else {
        return ConstValue::default();
    };
    match data {
        ExprData::Null => ConstValue {
            kind: ConstValueKind::Null,
            ..ConstValue::default()
        },
        ExprData::Undefined => ConstValue {
            kind: ConstValueKind::Undefined,
            ..ConstValue::default()
        },
        ExprData::Boolean(value) => ConstValue {
            kind: if *value {
                ConstValueKind::True
            } else {
                ConstValueKind::False
            },
            ..ConstValue::default()
        },
        ExprData::Number(value)
            if *value == (*value as i64) as f64 || value.to_string().chars().count() <= 8 =>
        {
            ConstValue {
                number: *value,
                kind: ConstValueKind::Number,
                ..ConstValue::default()
            }
        }
        ExprData::String(value) if value.value.len() <= 3 => ConstValue {
            string: value.value.clone(),
            kind: ConstValueKind::String,
            ..ConstValue::default()
        },
        _ => ConstValue::default(),
    }
}

/// # Panics
///
/// Panics if `value.kind` is `None`.
#[must_use]
pub fn const_value_to_expr(loc: Loc, value: &ConstValue) -> Expr {
    let data = match value.kind {
        ConstValueKind::Null => ExprData::Null,
        ConstValueKind::Undefined => ExprData::Undefined,
        ConstValueKind::True => ExprData::Boolean(true),
        ConstValueKind::False => ExprData::Boolean(false),
        ConstValueKind::Number => ExprData::Number(value.number),
        ConstValueKind::String => ExprData::String(StringExpr {
            value: value.string.clone(),
            ..StringExpr::default()
        }),
        ConstValueKind::None => panic!("internal error: invalid constant value"),
    };
    Expr::new(loc, data)
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
        AnnotationFlags, AssignTarget, CallExpr, CallKind, ConstValueKind, DotExpr, ExportsKind,
        Expr, ExprData, LocalKind, ModuleType, OP_TABLE, OpCode, OptionalChain, Precedence,
        PropertyFlags, PropertyKind, ScopeKind, StringExpr, const_value_to_expr,
        ensure_valid_identifier, expr_to_const_value, generate_non_unique_name_from_path,
    };
    use crate::internal::logger::Loc;

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

    #[test]
    fn expression_flags_and_constant_conversion_match_upstream() {
        let call = CallExpr {
            optional_chain: OptionalChain::Start,
            kind: CallKind::DirectEval,
            can_be_unwrapped_if_unused: true,
            ..CallExpr::default()
        };
        assert!(call.has_same_flags_as(&call.clone()));
        let mut other = call.clone();
        other.is_multi_line = true;
        assert!(call.has_same_flags_as(&other));
        other.optional_chain = OptionalChain::None;
        assert!(!call.has_same_flags_as(&other));

        let mut dot = DotExpr {
            can_be_removed_if_unused: true,
            ..DotExpr::default()
        };
        assert!(dot.has_same_flags_as(&dot.clone()));
        let mut other_dot = dot.clone();
        other_dot.name = "ignored".into();
        assert!(dot.has_same_flags_as(&other_dot));
        dot.is_symbol_instance = true;
        assert!(!dot.has_same_flags_as(&other_dot));

        let number = Expr::new(Loc::default(), ExprData::Number(123.0));
        let value = expr_to_const_value(&number);
        assert_eq!(value.kind, ConstValueKind::Number);
        assert!(matches!(
            const_value_to_expr(Loc::default(), &value).data.as_deref(),
            Some(ExprData::Number(123.0))
        ));

        let short_string = Expr::new(
            Loc::default(),
            ExprData::String(StringExpr {
                value: vec![1, 2, 3],
                ..StringExpr::default()
            }),
        );
        assert_eq!(
            expr_to_const_value(&short_string).kind,
            ConstValueKind::String
        );
        let long_string = Expr::new(
            Loc::default(),
            ExprData::String(StringExpr {
                value: vec![1, 2, 3, 4],
                ..StringExpr::default()
            }),
        );
        assert_eq!(expr_to_const_value(&long_string).kind, ConstValueKind::None);
        assert_eq!(
            expr_to_const_value(&Expr::new(Loc::default(), ExprData::Number(1e100))).kind,
            ConstValueKind::None
        );
    }
}
