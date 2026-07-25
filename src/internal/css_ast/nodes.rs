use crate::internal::ast::{CharFreq, ImportRecord, LocRef, Ref, Symbol, SymbolKind, SymbolMap};
use crate::internal::css_lexer::TokenKind;
use crate::internal::helpers::{hash_combine, hash_combine_string};
use crate::internal::logger::{Loc, Span};
use std::collections::HashMap;
use std::ops::{BitOr, BitOrAssign};

#[derive(Clone, Debug, Default)]
pub struct Ast {
    pub symbols: Vec<Symbol>,
    pub char_freq: Option<CharFreq>,
    pub import_records: Vec<ImportRecord>,
    pub rules: Vec<Rule>,
    pub source_map_comment: Span,
    pub approximate_line_count: i32,
    pub local_symbols: Vec<LocRef>,
    pub local_scope: HashMap<String, LocRef>,
    pub global_scope: HashMap<String, LocRef>,
    pub composes: HashMap<Ref, Composes>,
    pub layers_pre_import: Vec<Vec<String>>,
    pub layers_post_import: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct Composes {
    pub names: Vec<LocRef>,
    pub imported_names: Vec<ImportedComposesName>,
    pub properties: HashMap<String, Loc>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportedComposesName {
    pub alias: String,
    pub alias_loc: Loc,
    pub import_record_index: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Token {
    pub children: Option<Vec<Token>>,
    pub text: String,
    pub loc: Loc,
    pub payload_index: u32,
    pub unit_offset: u16,
    pub kind: TokenKind,
    pub whitespace: WhitespaceFlags,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WhitespaceFlags(u8);

impl WhitespaceFlags {
    pub const BEFORE: Self = Self(1 << 0);
    pub const AFTER: Self = Self(1 << 1);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for WhitespaceFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl BitOrAssign for WhitespaceFlags {
    fn bitor_assign(&mut self, right: Self) {
        self.0 |= right.0;
    }
}

pub struct CrossFileEqualityCheck<'a> {
    pub import_records_a: &'a [ImportRecord],
    pub import_records_b: &'a [ImportRecord],
    pub symbols: Option<&'a SymbolMap>,
    pub source_index_a: u32,
    pub source_index_b: u32,
}

impl CrossFileEqualityCheck<'_> {
    #[must_use]
    pub fn refs_are_equivalent(&self, mut left: Ref, mut right: Ref) -> bool {
        if left == right {
            return true;
        }
        let Some(symbols) = self.symbols else {
            return false;
        };
        left = symbols.follow_symbols_const(left);
        right = symbols.follow_symbols_const(right);
        if left == right {
            return true;
        }
        let left_symbol = symbols.get(left);
        let right_symbol = symbols.get(right);
        left_symbol.kind == SymbolKind::GlobalCss
            && right_symbol.kind == SymbolKind::GlobalCss
            && left_symbol.original_name == right_symbol.original_name
    }
}

impl Token {
    #[must_use]
    pub fn equal(&self, other: &Self, check: Option<&CrossFileEqualityCheck<'_>>) -> bool {
        if self.kind != other.kind || self.text != other.text || self.whitespace != other.whitespace
        {
            return false;
        }

        if self.kind == TokenKind::Url {
            if let Some(check) = check {
                let left = &check.import_records_a[self.payload_index as usize];
                let right = &check.import_records_b[other.payload_index as usize];
                if left.path.text != right.path.text {
                    return false;
                }
            } else if self.payload_index != other.payload_index {
                return false;
            }
        }

        if self.kind == TokenKind::Symbol {
            if let Some(check) = check {
                let left = Ref {
                    source_index: check.source_index_a,
                    inner_index: self.payload_index,
                };
                let right = Ref {
                    source_index: check.source_index_b,
                    inner_index: other.payload_index,
                };
                if !check.refs_are_equivalent(left, right) {
                    return false;
                }
            } else if self.payload_index != other.payload_index {
                return false;
            }
        }

        match (&self.children, &other.children) {
            (None, None) => true,
            (Some(left), Some(right)) => tokens_equal(left, right, check),
            _ => false,
        }
    }

    #[must_use]
    pub fn equal_ignoring_whitespace(&self, other: &Self) -> bool {
        if self.kind != other.kind
            || self.text != other.text
            || self.payload_index != other.payload_index
        {
            return false;
        }
        match (&self.children, &other.children) {
            (None, None) => true,
            (Some(left), Some(right)) => tokens_equal_ignoring_whitespace(left, right),
            _ => false,
        }
    }

    #[must_use]
    pub fn number_or_fraction_for_percentage(
        &self,
        percent_reference_range: f64,
        flags: PercentageFlags,
    ) -> Option<f64> {
        match self.kind {
            TokenKind::Number => self.text.parse().ok(),
            TokenKind::Percentage => {
                let percentage: f64 = self.percentage_value().parse().ok()?;
                if !flags.contains(PercentageFlags::ALLOW_BELOW_ZERO) && percentage < 0.0 {
                    return Some(0.0);
                }
                if !flags.contains(PercentageFlags::ALLOW_ABOVE_100) && percentage > 100.0 {
                    return Some(percent_reference_range);
                }
                Some(percentage / 100.0 * percent_reference_range)
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn clamped_fraction_for_percentage(&self) -> Option<f64> {
        if self.kind != TokenKind::Percentage {
            return None;
        }
        let percentage: f64 = self.percentage_value().parse().ok()?;
        Some((percentage / 100.0).clamp(0.0, 1.0))
    }

    pub fn turn_length_into_number_if_zero(&mut self) -> bool {
        if self.kind == TokenKind::Dimension && self.dimension_value() == "0" {
            self.kind = TokenKind::Number;
            self.text = "0".into();
            return true;
        }
        false
    }

    pub fn turn_length_or_percentage_into_number_if_zero(&mut self) -> bool {
        if self.kind == TokenKind::Percentage && self.percentage_value() == "0" {
            self.kind = TokenKind::Number;
            self.text = "0".into();
            return true;
        }
        self.turn_length_into_number_if_zero()
    }

    /// # Panics
    ///
    /// Panics if this token is not a non-empty percentage token.
    #[must_use]
    pub fn percentage_value(&self) -> &str {
        &self.text[..self.text.len() - 1]
    }

    /// # Panics
    ///
    /// Panics if `unit_offset` is outside the token text.
    #[must_use]
    pub fn dimension_value(&self) -> &str {
        &self.text[..usize::from(self.unit_offset)]
    }

    /// # Panics
    ///
    /// Panics if `unit_offset` is outside the token text.
    #[must_use]
    pub fn dimension_unit(&self) -> &str {
        &self.text[usize::from(self.unit_offset)..]
    }

    #[must_use]
    pub fn dimension_unit_is_safe_length(&self) -> bool {
        matches!(
            self.dimension_unit().to_ascii_lowercase().as_str(),
            "cm" | "em" | "in" | "mm" | "pc" | "pt" | "px"
        )
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.kind == TokenKind::Number && self.text == "0"
    }

    #[must_use]
    pub fn is_one(&self) -> bool {
        self.kind == TokenKind::Number && self.text == "1"
    }

    #[must_use]
    pub fn is_angle(&self) -> bool {
        self.kind == TokenKind::Dimension
            && matches!(
                self.dimension_unit().to_ascii_lowercase().as_str(),
                "deg" | "grad" | "rad" | "turn"
            )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PercentageFlags(u8);

impl PercentageFlags {
    pub const ALLOW_BELOW_ZERO: Self = Self(1 << 0);
    pub const ALLOW_ABOVE_100: Self = Self(1 << 1);
    pub const ALLOW_ANY: Self = Self(Self::ALLOW_BELOW_ZERO.0 | Self::ALLOW_ABOVE_100.0);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for PercentageFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

#[must_use]
pub fn tokens_equal(
    left: &[Token],
    right: &[Token],
    check: Option<&CrossFileEqualityCheck<'_>>,
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.equal(right, check))
}

#[must_use]
/// # Panics
///
/// Panics if a nested token list contains more than `u32::MAX` tokens.
pub fn hash_tokens(mut hash: u32, tokens: &[Token]) -> u32 {
    hash = hash_combine(
        hash,
        u32::try_from(tokens.len()).expect("token count must fit in 32 bits"),
    );
    for token in tokens {
        hash = hash_combine(hash, token.kind as u32);
        if token.kind != TokenKind::Url {
            hash = hash_combine_string(hash, &token.text);
        }
        if let Some(children) = &token.children {
            hash = hash_tokens(hash, children);
        }
    }
    hash
}

#[must_use]
pub fn tokens_equal_ignoring_whitespace(left: &[Token], right: &[Token]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.equal_ignoring_whitespace(right))
}

#[must_use]
pub fn tokens_are_comma_separated(tokens: &[Token]) -> bool {
    tokens.len() & 1 != 0
        && tokens
            .iter()
            .skip(1)
            .step_by(2)
            .all(|token| token.kind == TokenKind::Comma)
}

#[must_use]
pub fn clone_tokens_without_import_records(tokens: &[Token]) -> Vec<Token> {
    tokens.to_vec()
}

/// # Panics
///
/// Panics if a URL token refers to a missing input import record.
#[must_use]
pub fn clone_tokens_with_import_records(
    tokens: &[Token],
    import_records_in: &[ImportRecord],
    import_records_out: &mut Vec<ImportRecord>,
) -> Vec<Token> {
    tokens
        .iter()
        .cloned()
        .map(|mut token| {
            token.loc.start = 0;
            if token.kind == TokenKind::Url {
                let old_index = token.payload_index;
                token.payload_index = u32::try_from(import_records_out.len())
                    .expect("import record count must fit in 32 bits");
                import_records_out.push(import_records_in[old_index as usize].clone());
            }
            if let Some(children) = &token.children {
                token.children = Some(clone_tokens_with_import_records(
                    children,
                    import_records_in,
                    import_records_out,
                ));
            }
            token
        })
        .collect()
}

// Rule and selector node definitions follow below.

#[derive(Clone, Debug)]
pub struct Rule {
    pub data: RuleData,
    pub loc: Loc,
}

#[derive(Clone, Debug)]
pub enum RuleData {
    Placeholder,
}

#[derive(Clone, Debug)]
pub struct MediaQuery {
    pub loc: Loc,
    pub data: MediaQueryData,
}

#[derive(Clone, Debug)]
pub enum MediaQueryData {
    Placeholder,
}

#[cfg(test)]
mod tests {
    use super::{
        CrossFileEqualityCheck, PercentageFlags, Token, WhitespaceFlags,
        clone_tokens_with_import_records, hash_tokens, tokens_are_comma_separated,
    };
    use crate::internal::ast::{ImportRecord, Ref, Symbol, SymbolKind, SymbolMap};
    use crate::internal::css_lexer::TokenKind;

    #[test]
    fn token_percentage_and_dimension_helpers_match_css_semantics() {
        let percentage = Token {
            kind: TokenKind::Percentage,
            text: "125%".into(),
            ..Token::default()
        };
        assert_eq!(
            percentage.number_or_fraction_for_percentage(10.0, PercentageFlags::default()),
            Some(10.0)
        );
        assert_eq!(
            percentage.number_or_fraction_for_percentage(10.0, PercentageFlags::ALLOW_ABOVE_100),
            Some(12.5)
        );
        assert_eq!(percentage.clamped_fraction_for_percentage(), Some(1.0));

        let mut dimension = Token {
            kind: TokenKind::Dimension,
            text: "0PX".into(),
            unit_offset: 1,
            ..Token::default()
        };
        assert!(dimension.dimension_unit_is_safe_length());
        assert!(dimension.turn_length_into_number_if_zero());
        assert!(dimension.is_zero());
    }

    #[test]
    fn token_equality_handles_urls_symbols_children_and_whitespace() {
        let mut left_record = ImportRecord::default();
        left_record.path.text = "same.png".into();
        let mut right_record = ImportRecord::default();
        right_record.path.text = "same.png".into();
        let symbols = SymbolMap {
            symbols_for_source: vec![
                vec![Symbol::new(SymbolKind::GlobalCss, "same")],
                vec![Symbol::new(SymbolKind::GlobalCss, "same")],
            ],
        };
        let check = CrossFileEqualityCheck {
            import_records_a: &[left_record],
            import_records_b: &[right_record],
            symbols: Some(&symbols),
            source_index_a: 0,
            source_index_b: 1,
        };

        let left_url = Token {
            kind: TokenKind::Url,
            text: "left".into(),
            payload_index: 0,
            whitespace: WhitespaceFlags::AFTER,
            ..Token::default()
        };
        let right_url = Token {
            text: "left".into(),
            ..left_url.clone()
        };
        assert!(left_url.equal(&right_url, Some(&check)));
        assert_eq!(hash_tokens(0, &[left_url]), hash_tokens(0, &[right_url]));

        let left_symbol = Token {
            kind: TokenKind::Symbol,
            payload_index: 0,
            ..Token::default()
        };
        let right_symbol = left_symbol.clone();
        assert!(left_symbol.equal(&right_symbol, Some(&check)));
        assert!(check.refs_are_equivalent(
            Ref {
                source_index: 0,
                inner_index: 0
            },
            Ref {
                source_index: 1,
                inner_index: 0
            }
        ));
    }

    #[test]
    fn cloning_tokens_reindexes_nested_url_imports() {
        let input = vec![ImportRecord::default(), ImportRecord::default()];
        let tokens = vec![Token {
            children: Some(vec![Token {
                kind: TokenKind::Url,
                payload_index: 1,
                ..Token::default()
            }]),
            ..Token::default()
        }];
        let mut output = vec![ImportRecord::default()];
        let cloned = clone_tokens_with_import_records(&tokens, &input, &mut output);
        assert_eq!(output.len(), 2);
        assert_eq!(cloned[0].children.as_ref().unwrap()[0].payload_index, 1);
        assert!(tokens_are_comma_separated(&[
            Token::default(),
            Token {
                kind: TokenKind::Comma,
                ..Token::default()
            },
            Token::default()
        ]));
    }
}
