#![allow(dead_code)]

use crate::internal::logger::Range;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeferredErrors {
    pub(crate) invalid_expr_default_value: Range,
    pub(crate) invalid_expr_after_question: Range,
    pub(crate) array_spread_feature: Range,
    pub(crate) invalid_parens: Vec<Range>,
}

impl DeferredErrors {
    pub(crate) fn merge_into(self, target: &mut Self) {
        if self.invalid_expr_default_value.len > 0 {
            target.invalid_expr_default_value = self.invalid_expr_default_value;
        }
        if self.invalid_expr_after_question.len > 0 {
            target.invalid_expr_after_question = self.invalid_expr_after_question;
        }
        if self.array_spread_feature.len > 0 {
            target.array_spread_feature = self.array_spread_feature;
        }
        if !self.invalid_parens.is_empty() {
            target.invalid_parens.extend(self.invalid_parens);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeferredArrowArgErrors {
    pub(crate) invalid_expr_await: Range,
    pub(crate) invalid_expr_yield: Range,
}

#[cfg(test)]
mod tests {
    use super::DeferredErrors;
    use crate::internal::logger::{Loc, Range};

    fn range(start: i32) -> Range {
        Range {
            loc: Loc { start },
            len: 1,
        }
    }

    #[test]
    fn merge_overwrites_present_singletons_and_appends_parentheses() {
        let mut target = DeferredErrors {
            invalid_expr_default_value: range(1),
            invalid_parens: vec![range(2)],
            ..DeferredErrors::default()
        };
        DeferredErrors {
            invalid_expr_default_value: range(3),
            invalid_expr_after_question: range(4),
            invalid_parens: vec![range(5), range(6)],
            ..DeferredErrors::default()
        }
        .merge_into(&mut target);

        assert_eq!(target.invalid_expr_default_value, range(3));
        assert_eq!(target.invalid_expr_after_question, range(4));
        assert_eq!(target.invalid_parens, [range(2), range(5), range(6)]);
    }
}
