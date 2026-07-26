#![allow(dead_code)]

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum AwaitOrYield {
    #[default]
    AllowIdentifier,
    AllowExpression,
    ForbidAll,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct FnOrArrowDataParse {
    pub(crate) await_policy: AwaitOrYield,
    pub(crate) yield_policy: AwaitOrYield,
    pub(crate) allow_super_call: bool,
    pub(crate) allow_super_property: bool,
    pub(crate) is_this_disallowed: bool,
    pub(crate) is_return_disallowed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExprFlags(u8);

impl ExprFlags {
    pub(crate) const NONE: Self = Self(0);
    pub(crate) const DECORATOR: Self = Self(1 << 0);
    pub(crate) const FOR_LOOP_INIT: Self = Self(1 << 1);
    pub(crate) const FOR_AWAIT_LOOP_INIT: Self = Self(1 << 2);
    pub(crate) const AFTER_QUESTION_AND_BEFORE_COLON: Self = Self(1 << 3);
    pub(crate) const IS_NEW_TARGET: Self = Self(1 << 4);

    pub(crate) const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl std::ops::BitOr for ExprFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum FnKind {
    #[default]
    Statement,
    Expression,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum JsxImport {
    #[default]
    Jsx,
    Jsxs,
    Fragment,
    CreateElement,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ParenExprOptions {
    pub(crate) async_range: crate::internal::logger::Range,
    pub(crate) force_arrow_fn: bool,
    pub(crate) is_after_question_and_before_colon: bool,
}

#[cfg(test)]
mod tests {
    use super::ExprFlags;

    #[test]
    fn expression_context_flags_compose_like_upstream_bit_flags() {
        let flags = ExprFlags::DECORATOR | ExprFlags::FOR_LOOP_INIT;
        assert!(flags.contains(ExprFlags::DECORATOR));
        assert!(flags.contains(ExprFlags::FOR_LOOP_INIT));
        assert!(!flags.contains(ExprFlags::IS_NEW_TARGET));
    }
}
