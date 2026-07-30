// Port of upstream internal/ast/ast.go.

use crate::internal::helpers::{GlobPart, utf16_equals_wtf8};
use crate::internal::logger::{Loc, Path, Range};
use std::ops::{BitOr, BitOrAssign};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum ImportKind {
    #[default]
    EntryPoint,
    Stmt,
    Require,
    Dynamic,
    RequireResolve,
    At,
    ComposesFrom,
    Url,
}

impl ImportKind {
    #[must_use]
    pub const fn string_for_metafile(self) -> &'static str {
        match self {
            Self::Stmt => "import-statement",
            Self::Require => "require-call",
            Self::Dynamic => "dynamic-import",
            Self::RequireResolve => "require-resolve",
            Self::At => "import-rule",
            Self::ComposesFrom => "composes-from",
            Self::Url => "url-token",
            Self::EntryPoint => "entry-point",
        }
    }

    #[must_use]
    pub const fn is_from_css(self) -> bool {
        matches!(self, Self::At | Self::ComposesFrom | Self::Url)
    }

    #[must_use]
    pub const fn must_resolve_to_css(self) -> bool {
        matches!(self, Self::At | Self::ComposesFrom)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum ImportPhase {
    #[default]
    Evaluation,
    Defer,
    Source,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportRecordFlags(u16);

impl ImportRecordFlags {
    pub const IS_UNUSED: Self = Self(1 << 0);
    pub const CONTAINS_IMPORT_STAR: Self = Self(1 << 1);
    pub const CONTAINS_DEFAULT_ALIAS: Self = Self(1 << 2);
    pub const CONTAINS_ES_MODULE_ALIAS: Self = Self(1 << 3);
    pub const CALLS_RUN_TIME_RE_EXPORT_FN: Self = Self(1 << 4);
    pub const WRAP_WITH_TO_ESM: Self = Self(1 << 5);
    pub const WRAP_WITH_TO_CJS: Self = Self(1 << 6);
    pub const CALL_RUNTIME_REQUIRE: Self = Self(1 << 7);
    pub const HANDLES_IMPORT_ERRORS: Self = Self(1 << 8);
    pub const WAS_ORIGINALLY_BARE_IMPORT: Self = Self(1 << 9);
    pub const IS_EXTERNAL_WITHOUT_SIDE_EFFECTS: Self = Self(1 << 10);
    pub const ASSERT_TYPE_JSON: Self = Self(1 << 11);
    pub const SHOULD_NOT_BE_EXTERNAL_IN_METAFILE: Self = Self(1 << 12);
    pub const WAS_LOADED_WITH_EMPTY_LOADER: Self = Self(1 << 13);
    pub const CONTAINS_UNIQUE_KEY: Self = Self(1 << 14);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for ImportRecordFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ImportRecordFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportRecord {
    pub assert_or_with: Option<ImportAssertOrWith>,
    pub glob_pattern: Option<GlobPattern>,
    pub path: Path,
    pub range: Range,
    pub error_handler_loc: Loc,
    pub source_index: Index32,
    pub copy_source_index: Index32,
    pub flags: ImportRecordFlags,
    pub phase: ImportPhase,
    pub kind: ImportKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AssertOrWithKeyword {
    #[default]
    Assert,
    With,
}

impl AssertOrWithKeyword {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Assert => "assert",
            Self::With => "with",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportAssertOrWith {
    pub entries: Vec<AssertOrWithEntry>,
    pub keyword_loc: Loc,
    pub inner_open_brace_loc: Loc,
    pub inner_close_brace_loc: Loc,
    pub outer_open_brace_loc: Loc,
    pub outer_close_brace_loc: Loc,
    pub keyword: AssertOrWithKeyword,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssertOrWithEntry {
    pub key: Vec<u16>,
    pub value: Vec<u16>,
    pub key_loc: Loc,
    pub value_loc: Loc,
    pub prefer_quoted_key: bool,
}

#[must_use]
pub fn find_assert_or_with_entry<'a>(
    assertions: &'a [AssertOrWithEntry],
    name: &str,
) -> Option<&'a AssertOrWithEntry> {
    assertions
        .iter()
        .find(|assertion| utf16_equals_wtf8(&assertion.key, name.as_bytes()))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GlobPattern {
    pub parts: Vec<GlobPart>,
    pub export_alias: String,
    pub kind: ImportKind,
}

/// A 32-bit index whose zero value is invalid.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Index32 {
    flipped_bits: u32,
}

impl Index32 {
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self {
            flipped_bits: !index,
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.flipped_bits != 0
    }

    #[must_use]
    pub const fn get_index(self) -> u32 {
        !self.flipped_bits
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum SymbolKind {
    #[default]
    Unbound,
    Hoisted,
    HoistedFunction,
    CatchIdentifier,
    GeneratorOrAsyncFunction,
    Arguments,
    Class,
    ClassInComputedPropertyKey,
    PrivateField,
    PrivateMethod,
    PrivateGet,
    PrivateSet,
    PrivateGetSetPair,
    PrivateStaticField,
    PrivateStaticMethod,
    PrivateStaticGet,
    PrivateStaticSet,
    PrivateStaticGetSetPair,
    Label,
    TsEnum,
    TsNamespace,
    Import,
    Const,
    Injected,
    MangledProp,
    GlobalCss,
    LocalCss,
    Other,
}

impl SymbolKind {
    #[must_use]
    pub const fn is_private(self) -> bool {
        (self as u8) >= (Self::PrivateField as u8)
            && (self as u8) <= (Self::PrivateStaticGetSetPair as u8)
    }

    #[must_use]
    pub const fn is_hoisted(self) -> bool {
        matches!(self, Self::Hoisted | Self::HoistedFunction)
    }

    #[must_use]
    pub const fn is_hoisted_or_function(self) -> bool {
        self.is_hoisted() || matches!(self, Self::GeneratorOrAsyncFunction)
    }

    #[must_use]
    pub const fn is_function(self) -> bool {
        matches!(self, Self::HoistedFunction | Self::GeneratorOrAsyncFunction)
    }

    #[must_use]
    pub const fn is_unbound_or_injected(self) -> bool {
        matches!(self, Self::Unbound | Self::Injected)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Ref {
    pub source_index: u32,
    pub inner_index: u32,
}

pub const INVALID_REF: Ref = Ref {
    source_index: u32::MAX,
    inner_index: u32::MAX,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocRef {
    pub loc: Loc,
    pub reference: Ref,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ImportItemStatus {
    #[default]
    None,
    Generated,
    Missing,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SymbolFlags(u16);

impl SymbolFlags {
    pub const MUST_NOT_BE_RENAMED: Self = Self(1 << 0);
    pub const MUST_START_WITH_CAPITAL_LETTER_FOR_JSX: Self = Self(1 << 1);
    pub const DID_KEEP_NAME: Self = Self(1 << 2);
    pub const PRIVATE_SYMBOL_MUST_BE_LOWERED: Self = Self(1 << 3);
    pub const REMOVE_OVERWRITTEN_FUNCTION_DECLARATION: Self = Self(1 << 4);
    pub const DID_WARN_ABOUT_COMMON_JS_IN_ESM: Self = Self(1 << 5);
    pub const COULD_POTENTIALLY_BE_MUTATED: Self = Self(1 << 6);
    pub const WAS_EXPORTED: Self = Self(1 << 7);
    pub const IS_EMPTY_FUNCTION: Self = Self(1 << 8);
    pub const IS_IDENTITY_FUNCTION: Self = Self(1 << 9);
    pub const CALL_CAN_BE_UNWRAPPED_IF_UNUSED: Self = Self(1 << 10);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for SymbolFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for SymbolFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    pub namespace_alias: Option<NamespaceAlias>,
    pub original_name: String,
    pub link: Ref,
    pub use_count_estimate: u32,
    pub chunk_index: Index32,
    pub nested_scope_slot: Index32,
    pub flags: SymbolFlags,
    pub kind: SymbolKind,
    pub import_item_status: ImportItemStatus,
}

impl Symbol {
    #[must_use]
    pub fn new(kind: SymbolKind, original_name: impl Into<String>) -> Self {
        Self {
            kind,
            original_name: original_name.into(),
            ..Self::default()
        }
    }

    pub fn merge_contents_with(&mut self, old_symbol: &Self) {
        self.use_count_estimate = self
            .use_count_estimate
            .wrapping_add(old_symbol.use_count_estimate);
        if old_symbol.flags.contains(SymbolFlags::MUST_NOT_BE_RENAMED)
            && !self.flags.contains(SymbolFlags::MUST_NOT_BE_RENAMED)
        {
            self.original_name.clone_from(&old_symbol.original_name);
            self.flags |= SymbolFlags::MUST_NOT_BE_RENAMED;
        }
        if old_symbol
            .flags
            .contains(SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX)
        {
            self.flags |= SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX;
        }
    }

    #[must_use]
    pub const fn slot_namespace(&self) -> SlotNamespace {
        if matches!(self.kind, SymbolKind::Unbound)
            || self.flags.contains(SymbolFlags::MUST_NOT_BE_RENAMED)
        {
            SlotNamespace::MustNotBeRenamed
        } else if self.kind.is_private() {
            SlotNamespace::PrivateName
        } else if matches!(self.kind, SymbolKind::Label) {
            SlotNamespace::Label
        } else if matches!(self.kind, SymbolKind::MangledProp) {
            SlotNamespace::MangledProp
        } else {
            SlotNamespace::Default
        }
    }
}

impl Default for Symbol {
    fn default() -> Self {
        Self {
            namespace_alias: None,
            original_name: String::new(),
            link: INVALID_REF,
            use_count_estimate: 0,
            chunk_index: Index32::default(),
            nested_scope_slot: Index32::default(),
            flags: SymbolFlags::default(),
            kind: SymbolKind::default(),
            import_item_status: ImportItemStatus::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum SlotNamespace {
    #[default]
    Default,
    Label,
    PrivateName,
    MangledProp,
    MustNotBeRenamed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SlotCounts(pub [u32; 4]);

impl SlotCounts {
    pub fn union_max(&mut self, other: Self) {
        for (left, right) in self.0.iter_mut().zip(other.0) {
            *left = (*left).max(right);
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NamespaceAlias {
    pub alias: String,
    pub namespace_ref: Ref,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SymbolMap {
    pub symbols_for_source: Vec<Vec<Symbol>>,
}

impl SymbolMap {
    #[must_use]
    pub fn new(source_count: usize) -> Self {
        Self {
            symbols_for_source: vec![Vec::new(); source_count],
        }
    }

    /// # Panics
    ///
    /// Panics if `reference` is outside this symbol map.
    #[must_use]
    pub fn get(&self, reference: Ref) -> &Symbol {
        &self.symbols_for_source[reference.source_index as usize][reference.inner_index as usize]
    }

    /// # Panics
    ///
    /// Panics if `reference` is outside this symbol map.
    pub fn get_mut(&mut self, reference: Ref) -> &mut Symbol {
        &mut self.symbols_for_source[reference.source_index as usize]
            [reference.inner_index as usize]
    }

    /// Follows symbol links without path compression.
    ///
    /// # Panics
    ///
    /// Panics if any traversed reference is outside this symbol map.
    #[must_use]
    pub fn follow_symbols_const(&self, mut reference: Ref) -> Ref {
        loop {
            let link = self.get(reference).link;
            if link == INVALID_REF {
                return reference;
            }
            reference = link;
        }
    }

    /// Returns the canonical reference and compresses the traversed path.
    ///
    /// # Panics
    ///
    /// Panics if any traversed reference is outside this symbol map.
    pub fn follow_symbols(&mut self, reference: Ref) -> Ref {
        let link = self.get(reference).link;
        if link == INVALID_REF {
            return reference;
        }
        let canonical = self.follow_symbols(link);
        if link != canonical {
            self.get_mut(reference).link = canonical;
        }
        canonical
    }

    /// # Panics
    ///
    /// Panics if this map contains more sources or per-source symbols than fit
    /// in esbuild's 32-bit reference representation.
    pub fn follow_all_symbols(&mut self) {
        for source_index in 0..self.symbols_for_source.len() {
            for inner_index in 0..self.symbols_for_source[source_index].len() {
                self.follow_symbols(Ref {
                    source_index: u32::try_from(source_index)
                        .expect("source index must fit in 32 bits"),
                    inner_index: u32::try_from(inner_index)
                        .expect("symbol index must fit in 32 bits"),
                });
            }
        }
    }

    /// Merges `old` into `new` and returns the canonical reference.
    ///
    /// # Panics
    ///
    /// Panics if either reference or any traversed reference is outside this
    /// symbol map.
    pub fn merge_symbols(&mut self, old: Ref, new: Ref) -> Ref {
        if old == new {
            return new;
        }
        let old_link = self.get(old).link;
        if old_link != INVALID_REF {
            let merged = self.merge_symbols(old_link, new);
            self.get_mut(old).link = merged;
            return merged;
        }
        let new_link = self.get(new).link;
        if new_link != INVALID_REF {
            let merged = self.merge_symbols(old, new_link);
            self.get_mut(new).link = merged;
            return merged;
        }

        let old_symbol = self.get(old).clone();
        self.get_mut(old).link = new;
        self.get_mut(new).merge_contents_with(&old_symbol);
        new
    }
}

/// Histogram of identifier character frequencies for minification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CharFreq(pub [i32; 64]);

impl CharFreq {
    pub fn scan(&mut self, text: &[u8], delta: i32) {
        if delta == 0 {
            return;
        }
        for byte in text {
            let index = match byte {
                b'a'..=b'z' => usize::from(*byte - b'a'),
                b'A'..=b'Z' => usize::from(*byte - (b'A' - 26)),
                b'0'..=b'9' => usize::from(*byte + (52 - b'0')),
                b'_' => 62,
                b'$' => 63,
                _ => continue,
            };
            self.0[index] = self.0[index].wrapping_add(delta);
        }
    }

    pub fn include(&mut self, other: &Self) {
        for (value, other_value) in self.0.iter_mut().zip(other.0) {
            *value = value.wrapping_add(other_value);
        }
    }
}

impl Default for CharFreq {
    fn default() -> Self {
        Self([0; 64])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameMinifier {
    head: String,
    tail: String,
}

pub static DEFAULT_NAME_MINIFIER_JS: LazyLock<NameMinifier> = LazyLock::new(|| NameMinifier {
    head: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_$".to_string(),
    tail: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_$".to_string(),
});

pub static DEFAULT_NAME_MINIFIER_CSS: LazyLock<NameMinifier> = LazyLock::new(|| NameMinifier {
    head: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_".to_string(),
    tail: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_".to_string(),
});

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CharAndCount {
    character: u8,
    count: i32,
    index: u8,
}

impl NameMinifier {
    /// # Panics
    ///
    /// Panics if the minifier alphabet contains more than 256 characters.
    #[must_use]
    pub fn shuffle_by_char_freq(&self, frequency: CharFreq) -> Self {
        let mut array = vec![CharAndCount::default(); 64];
        for (index, character) in self.tail.bytes().enumerate() {
            array[index] = CharAndCount {
                character,
                index: u8::try_from(index).expect("minifier alphabet must fit in a byte"),
                count: frequency.0[index],
            };
        }
        array.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.index.cmp(&right.index))
        });

        let mut head = String::new();
        let mut tail = String::new();
        for item in array {
            if item.character == 0 {
                continue;
            }
            if !item.character.is_ascii_digit() {
                head.push(char::from(item.character));
            }
            tail.push(char::from(item.character));
        }
        Self { head, tail }
    }

    #[must_use]
    pub fn number_to_minified_name(&self, mut index: usize) -> String {
        let head_length = self.head.len();
        let tail_length = self.tail.len();
        let mut result = String::new();

        let mut character_index = index % head_length;
        result.push(char::from(self.head.as_bytes()[character_index]));
        index /= head_length;
        while index > 0 {
            index -= 1;
            character_index = index % tail_length;
            result.push(char::from(self.tail.as_bytes()[character_index]));
            index /= tail_length;
        }
        result
    }

    #[must_use]
    pub fn head(&self) -> &str {
        &self.head
    }

    #[must_use]
    pub fn tail(&self) -> &str {
        &self.tail
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AssertOrWithEntry, CharFreq, DEFAULT_NAME_MINIFIER_CSS, DEFAULT_NAME_MINIFIER_JS,
        ImportKind, ImportRecordFlags, Index32, Ref, SlotCounts, SlotNamespace, Symbol,
        SymbolFlags, SymbolKind, SymbolMap, find_assert_or_with_entry,
    };
    use crate::internal::helpers::string_to_utf16;

    #[test]
    fn import_kinds_and_flags_preserve_upstream_classification() {
        assert_eq!(ImportKind::Dynamic.string_for_metafile(), "dynamic-import");
        assert!(ImportKind::At.is_from_css());
        assert!(ImportKind::ComposesFrom.must_resolve_to_css());
        assert!(!ImportKind::Url.must_resolve_to_css());

        let flags =
            ImportRecordFlags::HANDLES_IMPORT_ERRORS | ImportRecordFlags::CONTAINS_UNIQUE_KEY;
        assert!(flags.contains(ImportRecordFlags::HANDLES_IMPORT_ERRORS));
        assert!(!flags.contains(ImportRecordFlags::IS_UNUSED));
    }

    #[test]
    fn index32_zero_value_is_invalid() {
        assert!(!Index32::default().is_valid());
        let index = Index32::new(123);
        assert!(index.is_valid());
        assert_eq!(index.get_index(), 123);
    }

    #[test]
    fn finds_utf16_import_attributes() {
        let entries = vec![AssertOrWithEntry {
            key: string_to_utf16(b"type"),
            value: string_to_utf16(b"json"),
            ..AssertOrWithEntry::default()
        }];
        assert!(find_assert_or_with_entry(&entries, "type").is_some());
        assert!(find_assert_or_with_entry(&entries, "mode").is_none());
    }

    #[test]
    fn symbol_merge_follows_and_compresses_links() {
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = vec![
            Symbol {
                original_name: "keep".into(),
                use_count_estimate: 2,
                flags: SymbolFlags::MUST_NOT_BE_RENAMED,
                ..Symbol::default()
            },
            Symbol {
                original_name: "new".into(),
                use_count_estimate: 3,
                ..Symbol::default()
            },
            Symbol::new(SymbolKind::Other, "third"),
        ];
        let first = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let second = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let third = Ref {
            source_index: 0,
            inner_index: 2,
        };
        assert_eq!(symbols.merge_symbols(first, second), second);
        assert_eq!(symbols.merge_symbols(second, third), third);
        assert_eq!(symbols.follow_symbols(first), third);
        assert_eq!(symbols.get(first).link, third);
        assert_eq!(symbols.get(third).use_count_estimate, 5);
        assert_eq!(symbols.get(third).original_name, "keep");
        assert!(
            symbols
                .get(third)
                .flags
                .contains(SymbolFlags::MUST_NOT_BE_RENAMED)
        );
    }

    #[test]
    fn symbol_slot_namespaces_and_counts_match_kind_rules() {
        assert_eq!(
            Symbol::new(SymbolKind::PrivateField, "#x").slot_namespace(),
            SlotNamespace::PrivateName
        );
        assert_eq!(
            Symbol::new(SymbolKind::Label, "loop").slot_namespace(),
            SlotNamespace::Label
        );
        let mut counts = SlotCounts([1, 5, 2, 0]);
        counts.union_max(SlotCounts([3, 4, 2, 9]));
        assert_eq!(counts.0, [3, 5, 2, 9]);
    }

    #[test]
    fn character_frequencies_accumulate_and_shuffle_names() {
        let mut frequency = CharFreq::default();
        frequency.scan(b"zzzzzaaZ9$", 1);
        let mut additional = CharFreq::default();
        additional.scan(b"z", 2);
        frequency.include(&additional);

        let minifier = DEFAULT_NAME_MINIFIER_JS.shuffle_by_char_freq(frequency);
        assert!(minifier.head().starts_with("zaZ$"));
        assert!(minifier.tail().starts_with("zaZ9$"));
        assert_eq!(minifier.number_to_minified_name(0), "z");
    }

    #[test]
    fn default_minified_name_sequence_matches_upstream() {
        assert_eq!(DEFAULT_NAME_MINIFIER_JS.number_to_minified_name(0), "a");
        assert_eq!(DEFAULT_NAME_MINIFIER_JS.number_to_minified_name(53), "$");
        assert_eq!(DEFAULT_NAME_MINIFIER_JS.number_to_minified_name(54), "aa");
        assert_eq!(DEFAULT_NAME_MINIFIER_CSS.number_to_minified_name(52), "_");
        assert_eq!(DEFAULT_NAME_MINIFIER_CSS.number_to_minified_name(53), "aa");
    }
}
