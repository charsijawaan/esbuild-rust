// Port of upstream internal/css_ast.

use crate::internal::helpers::TypoDetector;
use std::collections::HashMap;
use std::sync::LazyLock;

mod declarations;
mod nodes;

pub use declarations::Declaration;
pub use nodes::*;

pub static KNOWN_DECLARATIONS: LazyLock<HashMap<&'static str, Declaration>> = LazyLock::new(|| {
    declarations::KNOWN_DECLARATION_PAIRS
        .iter()
        .copied()
        .collect()
});

static TYPO_DETECTOR: LazyLock<TypoDetector> = LazyLock::new(|| {
    TypoDetector::new(
        &declarations::KNOWN_DECLARATION_PAIRS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>(),
    )
});

#[must_use]
pub fn maybe_correct_declaration_typo(text: &str) -> Option<&'static str> {
    if text.starts_with("--") {
        return None;
    }
    TYPO_DETECTOR.maybe_correct_typo(text)
}

#[cfg(test)]
mod tests {
    use super::{Declaration, KNOWN_DECLARATIONS, maybe_correct_declaration_typo};

    #[test]
    fn declaration_table_and_typo_correction_match_upstream_data() {
        assert_eq!(KNOWN_DECLARATIONS.len(), 328);
        assert_eq!(KNOWN_DECLARATIONS["appearance"], Declaration::Appearance);
        assert_eq!(KNOWN_DECLARATIONS["css-float"], Declaration::CssFloat);
        assert_eq!(
            maybe_correct_declaration_typo("backgroun-color"),
            Some("background-color")
        );
        assert_eq!(maybe_correct_declaration_typo("--backgroun-color"), None);
    }
}
