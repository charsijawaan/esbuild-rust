use super::Declaration;
use crate::internal::ast::{CharFreq, ImportRecord, LocRef, Ref, Symbol, SymbolKind, SymbolMap};
use crate::internal::css_lexer::TokenKind;
use crate::internal::helpers::{hash_combine, hash_combine_string};
use crate::internal::logger::{Loc, Range, Span};
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
    AtCharset(AtCharsetRule),
    AtImport(AtImportRule),
    AtKeyframes(AtKeyframesRule),
    KnownAt(KnownAtRule),
    UnknownAt(UnknownAtRule),
    Selector(SelectorRule),
    Qualified(QualifiedRule),
    Declaration(DeclarationRule),
    BadDeclaration(BadDeclarationRule),
    Comment(CommentRule),
    AtLayer(AtLayerRule),
    AtMedia(AtMediaRule),
    AtScope(AtScopeRule),
}

#[must_use]
pub fn rules_equal(
    left: &[Rule],
    right: &[Rule],
    check: Option<&CrossFileEqualityCheck<'_>>,
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.data.equal(&right.data, check))
}

#[must_use]
pub fn hash_rules(mut hash: u32, rules: &[Rule]) -> u32 {
    hash = hash_combine(hash, usize_to_u32(rules.len()));
    for rule in rules {
        hash = hash_combine(hash, rule.data.hash().unwrap_or(0));
    }
    hash
}

impl RuleData {
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn equal(&self, other: &Self, check: Option<&CrossFileEqualityCheck<'_>>) -> bool {
        match (self, other) {
            (Self::AtCharset(left), Self::AtCharset(right)) => left.encoding == right.encoding,
            (Self::AtKeyframes(left), Self::AtKeyframes(right)) => {
                left.at_token.eq_ignore_ascii_case(&right.at_token)
                    && refs_are_equivalent(check, left.name.reference, right.name.reference)
                    && left.blocks.len() == right.blocks.len()
                    && left.blocks.iter().zip(&right.blocks).all(|(left, right)| {
                        left.selectors == right.selectors
                            && rules_equal(&left.rules, &right.rules, check)
                    })
            }
            (Self::KnownAt(left), Self::KnownAt(right)) => {
                left.at_token.eq_ignore_ascii_case(&right.at_token)
                    && tokens_equal(&left.prelude, &right.prelude, check)
                    && rules_equal(&left.rules, &right.rules, check)
            }
            (Self::UnknownAt(left), Self::UnknownAt(right)) => {
                left.at_token.eq_ignore_ascii_case(&right.at_token)
                    && tokens_equal(&left.prelude, &right.prelude, check)
                    && tokens_equal(&left.block, &right.block, check)
            }
            (Self::Selector(left), Self::Selector(right)) => {
                complex_selectors_equal(&left.selectors, &right.selectors, check)
                    && rules_equal(&left.rules, &right.rules, check)
            }
            (Self::Qualified(left), Self::Qualified(right)) => {
                tokens_equal(&left.prelude, &right.prelude, check)
                    && rules_equal(&left.rules, &right.rules, check)
            }
            (Self::Declaration(left), Self::Declaration(right)) => {
                left.key_text == right.key_text
                    && tokens_equal(&left.value, &right.value, check)
                    && left.important == right.important
            }
            (Self::BadDeclaration(left), Self::BadDeclaration(right)) => {
                tokens_equal(&left.tokens, &right.tokens, check)
            }
            (Self::Comment(left), Self::Comment(right)) => left.text == right.text,

            (Self::AtMedia(left), Self::AtMedia(right)) => {
                media_queries_equal(&left.queries, &right.queries, check)
                    && rules_equal(&left.rules, &right.rules, check)
            }
            (Self::AtScope(left), Self::AtScope(right)) => {
                complex_selectors_equal(&left.start, &right.start, check)
                    && complex_selectors_equal(&left.end, &right.end, check)
                    && rules_equal(&left.rules, &right.rules, check)
            }
            // Import rules and, intentionally, layer rules are never considered
            // equal in upstream. Mismatched rule variants also land here.
            _ => false,
        }
    }

    #[must_use]
    pub fn hash(&self) -> Option<u32> {
        Some(match self {
            Self::AtCharset(rule) => hash_combine_string(1, &rule.encoding),
            Self::AtImport(_) => return None,
            Self::AtKeyframes(rule) => {
                let mut hash = hash_combine_string(2, &rule.at_token);
                hash = hash_combine(hash, usize_to_u32(rule.blocks.len()));
                for block in &rule.blocks {
                    hash = hash_combine(hash, usize_to_u32(block.selectors.len()));
                    for selector in &block.selectors {
                        hash = hash_combine_string(hash, selector);
                    }
                    hash = hash_rules(hash, &block.rules);
                }
                hash
            }
            Self::KnownAt(rule) => {
                let mut hash = hash_combine_string(3, &rule.at_token);
                hash = hash_tokens(hash, &rule.prelude);
                hash_rules(hash, &rule.rules)
            }
            Self::UnknownAt(rule) => {
                let mut hash = hash_combine_string(4, &rule.at_token);
                hash = hash_tokens(hash, &rule.prelude);
                hash_tokens(hash, &rule.block)
            }
            Self::Selector(rule) => {
                let mut hash = hash_combine(5, usize_to_u32(rule.selectors.len()));
                hash = hash_complex_selectors(hash, &rule.selectors);
                hash_rules(hash, &rule.rules)
            }
            Self::Qualified(rule) => {
                let hash = hash_tokens(6, &rule.prelude);
                hash_rules(hash, &rule.rules)
            }
            Self::Declaration(rule) => {
                let mut hash = if rule.key == Declaration::Unknown {
                    hash_combine_string(if rule.important { 7 } else { 8 }, &rule.key_text)
                } else {
                    hash_combine(if rule.important { 9 } else { 10 }, rule.key as u32)
                };
                hash = hash_tokens(hash, &rule.value);
                hash
            }
            Self::BadDeclaration(rule) => hash_tokens(7, &rule.tokens),
            Self::Comment(rule) => hash_combine_string(8, &rule.text),
            Self::AtLayer(rule) => {
                let mut hash = hash_combine(9, usize_to_u32(rule.names.len()));
                for parts in &rule.names {
                    hash = hash_combine(hash, usize_to_u32(parts.len()));
                    for part in parts {
                        hash = hash_combine_string(hash, part);
                    }
                }
                hash_rules(hash, &rule.rules)
            }
            Self::AtMedia(rule) => {
                let hash = hash_media_queries(10, &rule.queries);
                hash_rules(hash, &rule.rules)
            }
            Self::AtScope(rule) => {
                let mut hash = hash_complex_selectors(11, &rule.start);
                hash = hash_complex_selectors(hash, &rule.end);
                hash_rules(hash, &rule.rules)
            }
        })
    }
}

fn refs_are_equivalent(check: Option<&CrossFileEqualityCheck<'_>>, left: Ref, right: Ref) -> bool {
    left == right || check.is_some_and(|check| check.refs_are_equivalent(left, right))
}

fn usize_to_u32(value: usize) -> u32 {
    let bytes = value.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[derive(Clone, Debug, Default)]
pub struct AtCharsetRule {
    pub encoding: String,
}

#[derive(Clone, Debug, Default)]
pub struct ImportConditions {
    pub queries: Vec<MediaQuery>,
    pub layers: Vec<Token>,
    pub supports: Vec<Token>,
}

impl ImportConditions {
    /// # Panics
    ///
    /// Panics if a URL token refers to a missing input import record.
    #[must_use]
    pub fn clone_with_import_records(
        &self,
        import_records_in: &[ImportRecord],
        import_records_out: &mut Vec<ImportRecord>,
    ) -> Self {
        Self {
            layers: clone_tokens_with_import_records(
                &self.layers,
                import_records_in,
                import_records_out,
            ),
            supports: clone_tokens_with_import_records(
                &self.supports,
                import_records_in,
                import_records_out,
            ),
            queries: clone_media_queries_with_import_records(
                &self.queries,
                import_records_in,
                import_records_out,
            ),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AtImportRule {
    pub import_conditions: Option<ImportConditions>,
    pub import_record_index: u32,
}

#[derive(Clone, Debug, Default)]
pub struct AtKeyframesRule {
    pub at_token: String,
    pub name: LocRef,
    pub blocks: Vec<KeyframeBlock>,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct KeyframeBlock {
    pub selectors: Vec<String>,
    pub rules: Vec<Rule>,
    pub loc: Loc,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct KnownAtRule {
    pub at_token: String,
    pub prelude: Vec<Token>,
    pub rules: Vec<Rule>,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct UnknownAtRule {
    pub at_token: String,
    pub prelude: Vec<Token>,
    pub block: Vec<Token>,
}

#[derive(Clone, Debug, Default)]
pub struct SelectorRule {
    pub selectors: Vec<ComplexSelector>,
    pub rules: Vec<Rule>,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct QualifiedRule {
    pub prelude: Vec<Token>,
    pub rules: Vec<Rule>,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct DeclarationRule {
    pub key_text: String,
    pub value: Vec<Token>,
    pub key_range: Range,
    pub key: Declaration,
    pub important: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BadDeclarationRule {
    pub tokens: Vec<Token>,
}

#[derive(Clone, Debug, Default)]
pub struct CommentRule {
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct AtLayerRule {
    pub names: Vec<Vec<String>>,
    pub rules: Vec<Rule>,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct AtMediaRule {
    pub queries: Vec<MediaQuery>,
    pub rules: Vec<Rule>,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct AtScopeRule {
    pub start: Vec<ComplexSelector>,
    pub end: Vec<ComplexSelector>,
    pub rules: Vec<Rule>,
    pub close_brace_loc: Loc,
}

#[derive(Clone, Debug)]
pub struct MediaQuery {
    pub loc: Loc,
    pub data: MediaQueryData,
}

#[derive(Clone, Debug)]
pub enum MediaQueryData {
    Type(MediaTypeQuery),
    Not(MediaNotQuery),
    Binary(MediaBinaryQuery),
    ArbitraryTokens(MediaArbitraryTokensQuery),
    PlainOrBoolean(MediaPlainOrBooleanQuery),
    Range(MediaRangeQuery),
}

#[must_use]
pub fn media_queries_equal(
    left: &[MediaQuery],
    right: &[MediaQuery],
    check: Option<&CrossFileEqualityCheck<'_>>,
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.data.equal(&right.data, check))
}

#[must_use]
pub fn media_queries_equal_ignoring_whitespace(left: &[MediaQuery], right: &[MediaQuery]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.data.equal_ignoring_whitespace(&right.data))
}

#[must_use]
pub fn hash_media_queries(mut hash: u32, queries: &[MediaQuery]) -> u32 {
    hash = hash_combine(hash, usize_to_u32(queries.len()));
    for query in queries {
        hash = hash_combine(hash, query.data.hash());
    }
    hash
}

/// # Panics
///
/// Panics if a URL token refers to a missing input import record.
#[must_use]
pub fn clone_media_queries_with_import_records(
    queries: &[MediaQuery],
    import_records_in: &[ImportRecord],
    import_records_out: &mut Vec<ImportRecord>,
) -> Vec<MediaQuery> {
    queries
        .iter()
        .map(|query| MediaQuery {
            loc: Loc::default(),
            data: query
                .data
                .clone_with_import_records(import_records_in, import_records_out),
        })
        .collect()
}

impl MediaQueryData {
    #[must_use]
    pub fn equal(&self, other: &Self, check: Option<&CrossFileEqualityCheck<'_>>) -> bool {
        match (self, other) {
            (Self::Type(left), Self::Type(right)) => {
                left.op == right.op
                    && left.media_type == right.media_type
                    && match (&left.and_or_null, &right.and_or_null) {
                        (None, None) => true,
                        (Some(left), Some(right)) => left.data.equal(&right.data, check),
                        _ => false,
                    }
            }
            (Self::Not(left), Self::Not(right)) => left.inner.data.equal(&right.inner.data, check),
            (Self::Binary(left), Self::Binary(right)) => {
                left.op == right.op && media_queries_equal(&left.terms, &right.terms, check)
            }
            (Self::ArbitraryTokens(left), Self::ArbitraryTokens(right)) => {
                tokens_equal(&left.tokens, &right.tokens, check)
            }
            (Self::PlainOrBoolean(left), Self::PlainOrBoolean(right)) => {
                left.name == right.name
                    && tokens_equal(&left.value_or_nil, &right.value_or_nil, check)
            }
            (Self::Range(left), Self::Range(right)) => {
                left.before_cmp == right.before_cmp
                    && left.after_cmp == right.after_cmp
                    && left.name == right.name
                    && tokens_equal(&left.before, &right.before, check)
                    && tokens_equal(&left.after, &right.after, check)
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn equal_ignoring_whitespace(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Type(left), Self::Type(right)) => {
                left.op == right.op
                    && left.media_type == right.media_type
                    && match (&left.and_or_null, &right.and_or_null) {
                        (None, None) => true,
                        (Some(left), Some(right)) => {
                            left.data.equal_ignoring_whitespace(&right.data)
                        }
                        _ => false,
                    }
            }
            (Self::Not(left), Self::Not(right)) => {
                left.inner.data.equal_ignoring_whitespace(&right.inner.data)
            }
            (Self::Binary(left), Self::Binary(right)) => {
                left.op == right.op
                    && media_queries_equal_ignoring_whitespace(&left.terms, &right.terms)
            }
            (Self::ArbitraryTokens(left), Self::ArbitraryTokens(right)) => {
                tokens_equal_ignoring_whitespace(&left.tokens, &right.tokens)
            }
            (Self::PlainOrBoolean(left), Self::PlainOrBoolean(right)) => {
                left.name == right.name
                    && tokens_equal_ignoring_whitespace(&left.value_or_nil, &right.value_or_nil)
            }
            (Self::Range(left), Self::Range(right)) => {
                left.before_cmp == right.before_cmp
                    && left.after_cmp == right.after_cmp
                    && left.name == right.name
                    && tokens_equal_ignoring_whitespace(&left.before, &right.before)
                    && tokens_equal_ignoring_whitespace(&left.after, &right.after)
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn hash(&self) -> u32 {
        match self {
            Self::Type(query) => {
                let mut hash = hash_combine(0, query.op as u32);
                hash = hash_combine_string(hash, &query.media_type);
                if let Some(and_or_null) = &query.and_or_null {
                    hash = hash_combine(hash, and_or_null.data.hash());
                }
                hash
            }
            Self::Not(query) => hash_combine(1, query.inner.data.hash()),
            Self::Binary(query) => {
                let hash = hash_combine(2, query.op as u32);
                hash_media_queries(hash, &query.terms)
            }
            Self::ArbitraryTokens(query) => hash_tokens(3, &query.tokens),
            Self::PlainOrBoolean(query) => {
                let mut hash = hash_combine_string(4, &query.name);
                hash = hash_tokens(hash, &query.value_or_nil);
                hash
            }
            Self::Range(query) => {
                let mut hash = hash_tokens(5, &query.before);
                hash = hash_combine(hash, query.before_cmp as u32);
                hash = hash_combine_string(hash, &query.name);
                hash = hash_combine(hash, query.after_cmp as u32);
                hash_tokens(hash, &query.after)
            }
        }
    }

    /// # Panics
    ///
    /// Panics if a URL token refers to a missing input import record.
    #[must_use]
    pub fn clone_with_import_records(
        &self,
        import_records_in: &[ImportRecord],
        import_records_out: &mut Vec<ImportRecord>,
    ) -> Self {
        match self {
            Self::Type(query) => Self::Type(MediaTypeQuery {
                op: query.op,
                media_type: query.media_type.clone(),
                and_or_null: query.and_or_null.as_ref().map(|inner| {
                    Box::new(MediaQuery {
                        loc: Loc::default(),
                        data: inner
                            .data
                            .clone_with_import_records(import_records_in, import_records_out),
                    })
                }),
            }),
            Self::Not(query) => Self::Not(MediaNotQuery {
                inner: Box::new(MediaQuery {
                    loc: Loc::default(),
                    data: query
                        .inner
                        .data
                        .clone_with_import_records(import_records_in, import_records_out),
                }),
            }),
            Self::Binary(query) => Self::Binary(MediaBinaryQuery {
                op: query.op,
                terms: clone_media_queries_with_import_records(
                    &query.terms,
                    import_records_in,
                    import_records_out,
                ),
            }),
            Self::ArbitraryTokens(query) => Self::ArbitraryTokens(MediaArbitraryTokensQuery {
                tokens: clone_tokens_with_import_records(
                    &query.tokens,
                    import_records_in,
                    import_records_out,
                ),
            }),
            Self::PlainOrBoolean(query) => Self::PlainOrBoolean(MediaPlainOrBooleanQuery {
                name: query.name.clone(),
                value_or_nil: clone_tokens_with_import_records(
                    &query.value_or_nil,
                    import_records_in,
                    import_records_out,
                ),
            }),
            Self::Range(query) => Self::Range(MediaRangeQuery {
                before: clone_tokens_with_import_records(
                    &query.before,
                    import_records_in,
                    import_records_out,
                ),
                name: query.name.clone(),
                after: clone_tokens_with_import_records(
                    &query.after,
                    import_records_in,
                    import_records_out,
                ),
                name_loc: Loc::default(),
                before_cmp: query.before_cmp,
                after_cmp: query.after_cmp,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaTypeOp {
    #[default]
    None,
    Not,
    Only,
}

#[derive(Clone, Debug, Default)]
pub struct MediaTypeQuery {
    pub op: MediaTypeOp,
    pub media_type: String,
    pub and_or_null: Option<Box<MediaQuery>>,
}

#[derive(Clone, Debug)]
pub struct MediaNotQuery {
    pub inner: Box<MediaQuery>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaBinaryOp {
    #[default]
    And,
    Or,
}

#[derive(Clone, Debug, Default)]
pub struct MediaBinaryQuery {
    pub op: MediaBinaryOp,
    pub terms: Vec<MediaQuery>,
}

#[derive(Clone, Debug, Default)]
pub struct MediaArbitraryTokensQuery {
    pub tokens: Vec<Token>,
}

#[derive(Clone, Debug, Default)]
pub struct MediaPlainOrBooleanQuery {
    pub name: String,
    pub value_or_nil: Vec<Token>,
}

#[derive(Clone, Debug, Default)]
pub struct MediaRangeQuery {
    pub before: Vec<Token>,
    pub name: String,
    pub after: Vec<Token>,
    pub name_loc: Loc,
    pub before_cmp: MediaCmp,
    pub after_cmp: MediaCmp,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaCmp {
    #[default]
    None,
    Equal,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

impl MediaCmp {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LessThan => "<",
            Self::LessThanOrEqual => "<=",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => ">=",
            Self::None | Self::Equal => "=",
        }
    }

    #[must_use]
    pub const fn direction(self) -> i8 {
        match self {
            Self::LessThan | Self::LessThanOrEqual => -1,
            Self::GreaterThan | Self::GreaterThanOrEqual => 1,
            Self::None | Self::Equal => 0,
        }
    }

    #[must_use]
    pub const fn flip(self) -> Self {
        match self {
            Self::LessThan => Self::GreaterThanOrEqual,
            Self::LessThanOrEqual => Self::GreaterThan,
            Self::GreaterThan => Self::LessThanOrEqual,
            Self::GreaterThanOrEqual => Self::LessThan,
            Self::None | Self::Equal => self,
        }
    }

    #[must_use]
    pub const fn reverse(self) -> Self {
        match self {
            Self::LessThan => Self::GreaterThan,
            Self::LessThanOrEqual => Self::GreaterThanOrEqual,
            Self::GreaterThan => Self::LessThan,
            Self::GreaterThanOrEqual => Self::LessThanOrEqual,
            Self::None | Self::Equal => self,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ComplexSelector {
    pub selectors: Vec<CompoundSelector>,
}

#[must_use]
pub fn complex_selectors_equal(
    left: &[ComplexSelector],
    right: &[ComplexSelector],
    check: Option<&CrossFileEqualityCheck<'_>>,
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.equal(right, check))
}

#[must_use]
pub fn hash_complex_selectors(mut hash: u32, selectors: &[ComplexSelector]) -> u32 {
    for complex in selectors {
        hash = hash_combine(hash, usize_to_u32(complex.selectors.len()));
        for selector in &complex.selectors {
            if let Some(type_selector) = &selector.type_selector {
                hash = hash_combine_string(hash, &type_selector.name.text);
            } else {
                hash = hash_combine(hash, 0);
            }
            hash = hash_combine(hash, usize_to_u32(selector.subclass_selectors.len()));
            for subclass in &selector.subclass_selectors {
                hash = hash_combine(hash, subclass.data.hash());
            }
            hash = hash_combine(hash, u32::from(selector.combinator.byte));
        }
    }
    hash
}

impl ComplexSelector {
    #[must_use]
    pub fn contains_nesting_combinator(&self) -> bool {
        self.selectors.iter().any(|selector| {
            !selector.nesting_selector_locs.is_empty()
                || selector.subclass_selectors.iter().any(|subclass| {
                    matches!(
                        &subclass.data,
                        SubclassData::PseudoWithSelectorList(pseudo)
                            if pseudo
                                .selectors
                                .iter()
                                .any(Self::contains_nesting_combinator)
                    )
                })
        })
    }

    /// # Panics
    ///
    /// Panics if this complex selector is empty.
    #[must_use]
    pub fn is_relative(&self) -> bool {
        if self.selectors[0].combinator.byte == 0 && self.contains_nesting_combinator() {
            return false;
        }
        true
    }

    #[must_use]
    pub fn uses_pseudo_element(&self) -> bool {
        self.selectors.iter().any(|selector| {
            selector.subclass_selectors.iter().any(|subclass| {
                if let SubclassData::PseudoClass(pseudo) = &subclass.data {
                    pseudo.is_element
                        || matches!(
                            pseudo.name.as_str(),
                            "before" | "after" | "first-line" | "first-letter"
                        )
                } else {
                    false
                }
            })
        })
    }

    #[must_use]
    pub fn equal(&self, other: &Self, check: Option<&CrossFileEqualityCheck<'_>>) -> bool {
        self.selectors.len() == other.selectors.len()
            && self
                .selectors
                .iter()
                .zip(&other.selectors)
                .all(|(left, right)| {
                    left.nesting_selector_locs.len() == right.nesting_selector_locs.len()
                        && left.combinator.byte == right.combinator.byte
                        && match (&left.type_selector, &right.type_selector) {
                            (None, None) => true,
                            (Some(left), Some(right)) => left.equal(right),
                            _ => false,
                        }
                        && left.subclass_selectors.len() == right.subclass_selectors.len()
                        && left
                            .subclass_selectors
                            .iter()
                            .zip(&right.subclass_selectors)
                            .all(|(left, right)| left.data.equal(&right.data, check))
                })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Combinator {
    pub loc: Loc,
    pub byte: u8,
}

#[derive(Clone, Debug, Default)]
pub struct CompoundSelector {
    pub type_selector: Option<NamespacedName>,
    pub subclass_selectors: Vec<SubclassSelector>,
    pub nesting_selector_locs: Vec<Loc>,
    pub combinator: Combinator,
    pub was_empty_from_local_or_global: bool,
}

impl CompoundSelector {
    #[must_use]
    pub fn is_single_ampersand(&self) -> bool {
        self.nesting_selector_locs.len() == 1
            && self.combinator.byte == 0
            && self.type_selector.is_none()
            && self.subclass_selectors.is_empty()
    }

    #[must_use]
    pub fn is_invalid_because_empty(&self) -> bool {
        self.nesting_selector_locs.is_empty()
            && self.type_selector.is_none()
            && self.subclass_selectors.is_empty()
    }

    #[must_use]
    pub fn range(&self) -> Range {
        let mut range = if self.combinator.byte != 0 {
            Range {
                loc: self.combinator.loc,
                len: 1,
            }
        } else {
            Range::default()
        };
        if let Some(type_selector) = &self.type_selector {
            range.expand_by(type_selector.range());
        }
        for location in &self.nesting_selector_locs {
            range.expand_by(Range {
                loc: *location,
                len: 1,
            });
        }
        for subclass in &self.subclass_selectors {
            range.expand_by(subclass.range);
        }
        range
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NameToken {
    pub text: String,
    pub range: Range,
    pub kind: TokenKind,
}

impl NameToken {
    #[must_use]
    pub fn equal(&self, other: &Self) -> bool {
        self.text == other.text && self.kind == other.kind
    }
}

#[derive(Clone, Debug, Default)]
pub struct NamespacedName {
    pub namespace_prefix: Option<NameToken>,
    pub name: NameToken,
}

impl NamespacedName {
    #[must_use]
    pub fn range(&self) -> Range {
        if let Some(namespace_prefix) = &self.namespace_prefix {
            let location = namespace_prefix.range.loc;
            return Range {
                loc: location,
                len: self.name.range.end() - location.start,
            };
        }
        self.name.range
    }

    #[must_use]
    pub fn equal(&self, other: &Self) -> bool {
        self.name.equal(&other.name)
            && self.namespace_prefix.is_some() == other.namespace_prefix.is_some()
            && match (&self.namespace_prefix, &other.namespace_prefix) {
                // This intentionally mirrors upstream's conservative
                // comparison against the other selector's name.
                (Some(left), Some(_)) => left.equal(&other.name),
                _ => true,
            }
    }
}

#[derive(Clone, Debug)]
pub struct SubclassSelector {
    pub data: SubclassData,
    pub range: Range,
}

#[derive(Clone, Debug)]
pub enum SubclassData {
    Hash(HashSelector),
    Class(ClassSelector),
    Attribute(AttributeSelector),
    PseudoClass(PseudoClassSelector),
    PseudoWithSelectorList(PseudoClassWithSelectorList),
}

impl SubclassData {
    #[must_use]
    pub fn equal(&self, other: &Self, check: Option<&CrossFileEqualityCheck<'_>>) -> bool {
        match (self, other) {
            (Self::Hash(left), Self::Hash(right)) => {
                refs_are_equivalent(check, left.name.reference, right.name.reference)
            }
            (Self::Class(left), Self::Class(right)) => {
                refs_are_equivalent(check, left.name.reference, right.name.reference)
            }
            (Self::Attribute(left), Self::Attribute(right)) => {
                left.namespaced_name.equal(&right.namespaced_name)
                    && left.matcher_op == right.matcher_op
                    && left.matcher_value == right.matcher_value
                    && left.matcher_modifier == right.matcher_modifier
            }
            (Self::PseudoClass(left), Self::PseudoClass(right)) => {
                left.name == right.name
                    && tokens_equal(&left.args, &right.args, check)
                    && left.is_element == right.is_element
            }
            (Self::PseudoWithSelectorList(left), Self::PseudoWithSelectorList(right)) => {
                left.kind == right.kind
                    && left.index == right.index
                    && complex_selectors_equal(&left.selectors, &right.selectors, check)
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn hash(&self) -> u32 {
        match self {
            Self::Hash(_) => 1,
            Self::Class(_) => 2,
            Self::Attribute(selector) => {
                let mut hash = hash_combine_string(3, &selector.namespaced_name.name.text);
                hash = hash_combine_string(hash, &selector.matcher_op);
                hash_combine_string(hash, &selector.matcher_value)
            }
            Self::PseudoClass(selector) => {
                let hash = hash_combine_string(4, &selector.name);
                hash_tokens(hash, &selector.args)
            }
            Self::PseudoWithSelectorList(selector) => {
                let mut hash = hash_combine(5, selector.kind as u32);
                hash = hash_combine_string(hash, &selector.index.a);
                hash = hash_combine_string(hash, &selector.index.b);
                hash_complex_selectors(hash, &selector.selectors)
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HashSelector {
    pub name: LocRef,
}

#[derive(Clone, Debug, Default)]
pub struct ClassSelector {
    pub name: LocRef,
}

#[derive(Clone, Debug, Default)]
pub struct AttributeSelector {
    pub matcher_op: String,
    pub matcher_value: String,
    pub namespaced_name: NamespacedName,
    pub matcher_modifier: u8,
}

#[derive(Clone, Debug, Default)]
pub struct PseudoClassSelector {
    pub name: String,
    pub args: Vec<Token>,
    pub is_element: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PseudoClassKind {
    #[default]
    Global,
    Has,
    Is,
    Local,
    Not,
    NthChild,
    NthLastChild,
    NthLastOfType,
    NthOfType,
    Where,
}

impl PseudoClassKind {
    #[must_use]
    pub const fn has_nth_index(self) -> bool {
        matches!(
            self,
            Self::NthChild | Self::NthLastChild | Self::NthLastOfType | Self::NthOfType
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Has => "has",
            Self::Is => "is",
            Self::Local => "local",
            Self::Not => "not",
            Self::NthChild => "nth-child",
            Self::NthLastChild => "nth-last-child",
            Self::NthLastOfType => "nth-last-of-type",
            Self::NthOfType => "nth-of-type",
            Self::Where => "where",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NthIndex {
    pub a: String,
    pub b: String,
}

impl NthIndex {
    pub fn minify(&mut self) {
        if self.b == "even" {
            self.a = "2".into();
            self.b.clear();
            return;
        }
        if self.a == "2" && self.b == "1" {
            self.a.clear();
            self.b = "odd".into();
            return;
        }
        if self.a == "0" {
            self.a.clear();
            if self.b.is_empty() {
                self.b = "0".into();
            }
            return;
        }
        if self.b == "0" && !self.a.is_empty() {
            self.b.clear();
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PseudoClassWithSelectorList {
    pub selectors: Vec<ComplexSelector>,
    pub index: NthIndex,
    pub kind: PseudoClassKind,
}

#[must_use]
pub fn tokens_contain_ampersand_recursive(tokens: &[Token]) -> bool {
    tokens.iter().any(|token| {
        token.kind == TokenKind::DelimAmpersand
            || token
                .children
                .as_ref()
                .is_some_and(|children| tokens_contain_ampersand_recursive(children))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AtLayerRule, CommentRule, ComplexSelector, CompoundSelector, CrossFileEqualityCheck,
        MediaArbitraryTokensQuery, MediaCmp, MediaQuery, MediaQueryData, NameToken, NamespacedName,
        NthIndex, PercentageFlags, PseudoClassSelector, RuleData, SubclassData, SubclassSelector,
        Token, WhitespaceFlags, clone_media_queries_with_import_records,
        clone_tokens_with_import_records, hash_tokens, tokens_are_comma_separated,
        tokens_contain_ampersand_recursive,
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

    #[test]
    fn rule_and_media_query_semantics_match_upstream() {
        let left = RuleData::Comment(CommentRule {
            text: "legal".into(),
        });
        let right = left.clone();
        assert!(left.equal(&right, None));
        assert_eq!(left.hash(), right.hash());

        let layer = RuleData::AtLayer(AtLayerRule::default());
        assert!(!layer.equal(&layer, None));

        let query = MediaQuery {
            loc: crate::internal::logger::Loc { start: 12 },
            data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery {
                tokens: vec![Token {
                    kind: TokenKind::Url,
                    payload_index: 0,
                    loc: crate::internal::logger::Loc { start: 8 },
                    ..Token::default()
                }],
            }),
        };
        let mut output_records = Vec::new();
        let cloned = clone_media_queries_with_import_records(
            &[query],
            &[ImportRecord::default()],
            &mut output_records,
        );
        assert_eq!(cloned[0].loc.start, 0);
        let MediaQueryData::ArbitraryTokens(cloned_query) = &cloned[0].data else {
            panic!("expected arbitrary tokens");
        };
        assert_eq!(cloned_query.tokens[0].loc.start, 0);
        assert_eq!(output_records.len(), 1);
    }

    #[test]
    fn selector_and_nth_helpers_match_upstream() {
        let nested = ComplexSelector {
            selectors: vec![CompoundSelector {
                nesting_selector_locs: vec![crate::internal::logger::Loc { start: 2 }],
                ..CompoundSelector::default()
            }],
        };
        assert!(nested.contains_nesting_combinator());
        assert!(!nested.is_relative());

        let pseudo = ComplexSelector {
            selectors: vec![CompoundSelector {
                subclass_selectors: vec![SubclassSelector {
                    data: SubclassData::PseudoClass(PseudoClassSelector {
                        name: "before".into(),
                        ..PseudoClassSelector::default()
                    }),
                    range: crate::internal::logger::Range::default(),
                }],
                ..CompoundSelector::default()
            }],
        };
        assert!(pseudo.uses_pseudo_element());

        let token = Token {
            children: Some(vec![Token {
                kind: TokenKind::DelimAmpersand,
                ..Token::default()
            }]),
            ..Token::default()
        };
        assert!(tokens_contain_ampersand_recursive(&[token]));

        let mut index = NthIndex {
            a: "2".into(),
            b: "1".into(),
        };
        index.minify();
        assert_eq!((index.a.as_str(), index.b.as_str()), ("", "odd"));
        assert_eq!(MediaCmp::LessThan.flip(), MediaCmp::GreaterThanOrEqual);
        assert_eq!(MediaCmp::LessThan.reverse(), MediaCmp::GreaterThan);

        let namespaced = NamespacedName {
            namespace_prefix: Some(NameToken {
                text: "svg".into(),
                ..NameToken::default()
            }),
            name: NameToken {
                text: "a".into(),
                ..NameToken::default()
            },
        };
        assert!(!namespaced.equal(&namespaced));
    }
}
