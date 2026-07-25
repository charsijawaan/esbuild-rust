// Port of upstream internal/compat.

use crate::internal::ast::SymbolKind;
use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasher;
use std::ops::{BitOr, BitOrAssign, Not};
use std::sync::LazyLock;

mod js_table_data;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Version {
    major: u16,
    minor: u8,
    patch: u8,
}

impl Version {
    const fn new(major: u16, minor: u8, patch: u8) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Semver {
    pub parts: Vec<i32>,
    pub pre_release: String,
}

impl fmt::Display for Semver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, part) in self.parts.iter().enumerate() {
            if index > 0 {
                formatter.write_str(".")?;
            }
            write!(formatter, "{part}")?;
        }
        formatter.write_str(&self.pre_release)
    }
}

fn compare_versions(left: Version, right: &Semver) -> i32 {
    let mut difference = i32::from(left.major) - right.parts.first().copied().unwrap_or(0);
    if difference == 0 {
        difference = i32::from(left.minor) - right.parts.get(1).copied().unwrap_or(0);
    }
    if difference == 0 {
        difference = i32::from(left.patch) - right.parts.get(2).copied().unwrap_or(0);
    }
    if difference == 0 && !right.pre_release.is_empty() {
        return 1;
    }
    difference
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VersionRange {
    start: Version,
    end: Version,
}

type EngineRanges = &'static [(Engine, &'static [VersionRange])];
type FeatureTable = &'static [(JsFeature, EngineRanges)];

impl VersionRange {
    const fn from_start(major: u16, minor: u8, patch: u8) -> Self {
        Self {
            start: Version::new(major, minor, patch),
            end: Version::new(0, 0, 0),
        }
    }

    const fn bounded(
        start_major: u16,
        start_minor: u8,
        start_patch: u8,
        end_major: u16,
        end_minor: u8,
        end_patch: u8,
    ) -> Self {
        Self {
            start: Version::new(start_major, start_minor, start_patch),
            end: Version::new(end_major, end_minor, end_patch),
        }
    }
}

fn is_version_supported(ranges: &[VersionRange], version: &Semver) -> bool {
    ranges.iter().any(|range| {
        compare_versions(range.start, version) <= 0
            && (range.end == Version::default() || compare_versions(range.end, version) > 0)
    })
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum Engine {
    Chrome,
    Deno,
    Edge,
    Es,
    Firefox,
    Hermes,
    Ie,
    Ios,
    Node,
    Opera,
    Rhino,
    Safari,
}

impl Engine {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Deno => "deno",
            Self::Edge => "edge",
            Self::Es => "es",
            Self::Firefox => "firefox",
            Self::Hermes => "hermes",
            Self::Ie => "ie",
            Self::Ios => "ios",
            Self::Node => "node",
            Self::Opera => "opera",
            Self::Rhino => "rhino",
            Self::Safari => "safari",
        }
    }

    #[must_use]
    pub const fn is_browser(self) -> bool {
        matches!(
            self,
            Self::Chrome
                | Self::Edge
                | Self::Firefox
                | Self::Ie
                | Self::Ios
                | Self::Opera
                | Self::Safari
        )
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct JsFeature(u64);

impl JsFeature {
    pub const NONE: Self = Self(0);
    pub const ARBITRARY_MODULE_NAMESPACE_NAMES: Self = Self(1 << 0);
    pub const ARRAY_SPREAD: Self = Self(1 << 1);
    pub const ARROW: Self = Self(1 << 2);
    pub const ASYNC_AWAIT: Self = Self(1 << 3);
    pub const ASYNC_GENERATOR: Self = Self(1 << 4);
    pub const BIGINT: Self = Self(1 << 5);
    pub const CLASS: Self = Self(1 << 6);
    pub const CLASS_FIELD: Self = Self(1 << 7);
    pub const CLASS_PRIVATE_ACCESSOR: Self = Self(1 << 8);
    pub const CLASS_PRIVATE_BRAND_CHECK: Self = Self(1 << 9);
    pub const CLASS_PRIVATE_FIELD: Self = Self(1 << 10);
    pub const CLASS_PRIVATE_METHOD: Self = Self(1 << 11);
    pub const CLASS_PRIVATE_STATIC_ACCESSOR: Self = Self(1 << 12);
    pub const CLASS_PRIVATE_STATIC_FIELD: Self = Self(1 << 13);
    pub const CLASS_PRIVATE_STATIC_METHOD: Self = Self(1 << 14);
    pub const CLASS_STATIC_BLOCKS: Self = Self(1 << 15);
    pub const CLASS_STATIC_FIELD: Self = Self(1 << 16);
    pub const CONST_AND_LET: Self = Self(1 << 17);
    pub const DECORATORS: Self = Self(1 << 18);
    pub const DEFAULT_ARGUMENT: Self = Self(1 << 19);
    pub const DESTRUCTURING: Self = Self(1 << 20);
    pub const DYNAMIC_IMPORT: Self = Self(1 << 21);
    pub const EXPONENT_OPERATOR: Self = Self(1 << 22);
    pub const EXPORT_STAR_AS: Self = Self(1 << 23);
    pub const FOR_AWAIT: Self = Self(1 << 24);
    pub const FOR_OF: Self = Self(1 << 25);
    pub const FROM_BASE64: Self = Self(1 << 26);
    pub const FUNCTION_NAME_CONFIGURABLE: Self = Self(1 << 27);
    pub const FUNCTION_OR_CLASS_PROPERTY_ACCESS: Self = Self(1 << 28);
    pub const GENERATOR: Self = Self(1 << 29);
    pub const HASHBANG: Self = Self(1 << 30);
    pub const IMPORT_ASSERTIONS: Self = Self(1 << 31);
    pub const IMPORT_ATTRIBUTES: Self = Self(1 << 32);
    pub const IMPORT_DEFER: Self = Self(1 << 33);
    pub const IMPORT_META: Self = Self(1 << 34);
    pub const IMPORT_SOURCE: Self = Self(1 << 35);
    pub const INLINE_SCRIPT: Self = Self(1 << 36);
    pub const LOGICAL_ASSIGNMENT: Self = Self(1 << 37);
    pub const NESTED_REST_BINDING: Self = Self(1 << 38);
    pub const NEW_TARGET: Self = Self(1 << 39);
    pub const NODE_COLON_PREFIX_IMPORT: Self = Self(1 << 40);
    pub const NODE_COLON_PREFIX_REQUIRE: Self = Self(1 << 41);
    pub const NULLISH_COALESCING: Self = Self(1 << 42);
    pub const OBJECT_ACCESSORS: Self = Self(1 << 43);
    pub const OBJECT_EXTENSIONS: Self = Self(1 << 44);
    pub const OBJECT_REST_SPREAD: Self = Self(1 << 45);
    pub const OPTIONAL_CATCH_BINDING: Self = Self(1 << 46);
    pub const OPTIONAL_CHAIN: Self = Self(1 << 47);
    pub const REGEXP_DOT_ALL_FLAG: Self = Self(1 << 48);
    pub const REGEXP_LOOKBEHIND_ASSERTIONS: Self = Self(1 << 49);
    pub const REGEXP_MATCH_INDICES: Self = Self(1 << 50);
    pub const REGEXP_NAMED_CAPTURE_GROUPS: Self = Self(1 << 51);
    pub const REGEXP_SET_NOTATION: Self = Self(1 << 52);
    pub const REGEXP_STICKY_AND_UNICODE_FLAGS: Self = Self(1 << 53);
    pub const REGEXP_UNICODE_PROPERTY_ESCAPES: Self = Self(1 << 54);
    pub const REST_ARGUMENT: Self = Self(1 << 55);
    pub const TEMPLATE_LITERAL: Self = Self(1 << 56);
    pub const TOP_LEVEL_AWAIT: Self = Self(1 << 57);
    pub const TYPEOF_EXOTIC_OBJECT_IS_OBJECT: Self = Self(1 << 58);
    pub const UNICODE_ESCAPES: Self = Self(1 << 59);
    pub const USING: Self = Self(1 << 60);

    #[must_use]
    pub const fn contains(self, feature: Self) -> bool {
        self.0 & feature.0 != 0
    }

    #[must_use]
    pub const fn apply_overrides(self, overrides: Self, mask: Self) -> Self {
        Self((self.0 & !mask.0) | (overrides.0 & mask.0))
    }
}

impl BitOr for JsFeature {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl BitOrAssign for JsFeature {
    fn bitor_assign(&mut self, right: Self) {
        self.0 |= right.0;
    }
}

impl Not for JsFeature {
    type Output = Self;

    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

const JS_FEATURE_NAMES: &[(&str, JsFeature)] = &[
    (
        "arbitrary-module-namespace-names",
        JsFeature::ARBITRARY_MODULE_NAMESPACE_NAMES,
    ),
    ("array-spread", JsFeature::ARRAY_SPREAD),
    ("arrow", JsFeature::ARROW),
    ("async-await", JsFeature::ASYNC_AWAIT),
    ("async-generator", JsFeature::ASYNC_GENERATOR),
    ("bigint", JsFeature::BIGINT),
    ("class", JsFeature::CLASS),
    ("class-field", JsFeature::CLASS_FIELD),
    ("class-private-accessor", JsFeature::CLASS_PRIVATE_ACCESSOR),
    (
        "class-private-brand-check",
        JsFeature::CLASS_PRIVATE_BRAND_CHECK,
    ),
    ("class-private-field", JsFeature::CLASS_PRIVATE_FIELD),
    ("class-private-method", JsFeature::CLASS_PRIVATE_METHOD),
    (
        "class-private-static-accessor",
        JsFeature::CLASS_PRIVATE_STATIC_ACCESSOR,
    ),
    (
        "class-private-static-field",
        JsFeature::CLASS_PRIVATE_STATIC_FIELD,
    ),
    (
        "class-private-static-method",
        JsFeature::CLASS_PRIVATE_STATIC_METHOD,
    ),
    ("class-static-blocks", JsFeature::CLASS_STATIC_BLOCKS),
    ("class-static-field", JsFeature::CLASS_STATIC_FIELD),
    ("const-and-let", JsFeature::CONST_AND_LET),
    ("decorators", JsFeature::DECORATORS),
    ("default-argument", JsFeature::DEFAULT_ARGUMENT),
    ("destructuring", JsFeature::DESTRUCTURING),
    ("dynamic-import", JsFeature::DYNAMIC_IMPORT),
    ("exponent-operator", JsFeature::EXPONENT_OPERATOR),
    ("export-star-as", JsFeature::EXPORT_STAR_AS),
    ("for-await", JsFeature::FOR_AWAIT),
    ("for-of", JsFeature::FOR_OF),
    ("from-base64", JsFeature::FROM_BASE64),
    (
        "function-name-configurable",
        JsFeature::FUNCTION_NAME_CONFIGURABLE,
    ),
    (
        "function-or-class-property-access",
        JsFeature::FUNCTION_OR_CLASS_PROPERTY_ACCESS,
    ),
    ("generator", JsFeature::GENERATOR),
    ("hashbang", JsFeature::HASHBANG),
    ("import-assertions", JsFeature::IMPORT_ASSERTIONS),
    ("import-attributes", JsFeature::IMPORT_ATTRIBUTES),
    ("import-defer", JsFeature::IMPORT_DEFER),
    ("import-meta", JsFeature::IMPORT_META),
    ("import-source", JsFeature::IMPORT_SOURCE),
    ("inline-script", JsFeature::INLINE_SCRIPT),
    ("logical-assignment", JsFeature::LOGICAL_ASSIGNMENT),
    ("nested-rest-binding", JsFeature::NESTED_REST_BINDING),
    ("new-target", JsFeature::NEW_TARGET),
    (
        "node-colon-prefix-import",
        JsFeature::NODE_COLON_PREFIX_IMPORT,
    ),
    (
        "node-colon-prefix-require",
        JsFeature::NODE_COLON_PREFIX_REQUIRE,
    ),
    ("nullish-coalescing", JsFeature::NULLISH_COALESCING),
    ("object-accessors", JsFeature::OBJECT_ACCESSORS),
    ("object-extensions", JsFeature::OBJECT_EXTENSIONS),
    ("object-rest-spread", JsFeature::OBJECT_REST_SPREAD),
    ("optional-catch-binding", JsFeature::OPTIONAL_CATCH_BINDING),
    ("optional-chain", JsFeature::OPTIONAL_CHAIN),
    ("regexp-dot-all-flag", JsFeature::REGEXP_DOT_ALL_FLAG),
    (
        "regexp-lookbehind-assertions",
        JsFeature::REGEXP_LOOKBEHIND_ASSERTIONS,
    ),
    ("regexp-match-indices", JsFeature::REGEXP_MATCH_INDICES),
    (
        "regexp-named-capture-groups",
        JsFeature::REGEXP_NAMED_CAPTURE_GROUPS,
    ),
    ("regexp-set-notation", JsFeature::REGEXP_SET_NOTATION),
    (
        "regexp-sticky-and-unicode-flags",
        JsFeature::REGEXP_STICKY_AND_UNICODE_FLAGS,
    ),
    (
        "regexp-unicode-property-escapes",
        JsFeature::REGEXP_UNICODE_PROPERTY_ESCAPES,
    ),
    ("rest-argument", JsFeature::REST_ARGUMENT),
    ("template-literal", JsFeature::TEMPLATE_LITERAL),
    ("top-level-await", JsFeature::TOP_LEVEL_AWAIT),
    (
        "typeof-exotic-object-is-object",
        JsFeature::TYPEOF_EXOTIC_OBJECT_IS_OBJECT,
    ),
    ("unicode-escapes", JsFeature::UNICODE_ESCAPES),
    ("using", JsFeature::USING),
];

pub static STRING_TO_JS_FEATURE: LazyLock<HashMap<&'static str, JsFeature>> =
    LazyLock::new(|| JS_FEATURE_NAMES.iter().copied().collect());

#[must_use]
pub fn unsupported_js_features<S: BuildHasher>(
    constraints: &HashMap<Engine, Semver, S>,
) -> JsFeature {
    let mut unsupported = JsFeature::NONE;
    for (feature, engines) in js_table_data::JS_TABLE {
        if *feature == JsFeature::INLINE_SCRIPT {
            continue;
        }
        for (engine, version) in constraints {
            let ranges = engines
                .iter()
                .find_map(|(candidate, ranges)| (candidate == engine).then_some(*ranges));
            if ranges.is_none_or(|ranges| !is_version_supported(ranges, version)) {
                unsupported |= *feature;
            }
        }
    }
    unsupported
}

#[must_use]
pub const fn symbol_feature(kind: SymbolKind) -> JsFeature {
    match kind {
        SymbolKind::PrivateField => JsFeature::CLASS_PRIVATE_FIELD,
        SymbolKind::PrivateMethod => JsFeature::CLASS_PRIVATE_METHOD,
        SymbolKind::PrivateGet | SymbolKind::PrivateSet | SymbolKind::PrivateGetSetPair => {
            JsFeature::CLASS_PRIVATE_ACCESSOR
        }
        SymbolKind::PrivateStaticField => JsFeature::CLASS_PRIVATE_STATIC_FIELD,
        SymbolKind::PrivateStaticMethod => JsFeature::CLASS_PRIVATE_STATIC_METHOD,
        SymbolKind::PrivateStaticGet
        | SymbolKind::PrivateStaticSet
        | SymbolKind::PrivateStaticGetSetPair => JsFeature::CLASS_PRIVATE_STATIC_ACCESSOR,
        _ => JsFeature::NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Engine, JsFeature, STRING_TO_JS_FEATURE, Semver, Version, compare_versions,
        unsupported_js_features,
    };
    use std::cmp::Ordering;
    use std::collections::HashMap;

    fn compare(left: Version, right: &Semver) -> Ordering {
        compare_versions(left, right).cmp(&0)
    }

    #[test]
    fn compares_versions_like_upstream() {
        let cases = [
            (Version::new(0, 0, 0), &[][..], "", Ordering::Equal),
            (Version::new(1, 0, 0), &[][..], "", Ordering::Greater),
            (Version::new(0, 1, 0), &[][..], "", Ordering::Greater),
            (Version::new(0, 0, 1), &[][..], "", Ordering::Greater),
            (Version::new(0, 0, 0), &[1][..], "", Ordering::Less),
            (Version::new(0, 0, 0), &[0, 1][..], "", Ordering::Less),
            (Version::new(0, 0, 0), &[0, 0, 1][..], "", Ordering::Less),
            (Version::new(0, 4, 0), &[0, 5, 0][..], "", Ordering::Less),
            (Version::new(0, 5, 0), &[0, 5, 0][..], "", Ordering::Equal),
            (Version::new(0, 6, 0), &[0, 5, 0][..], "", Ordering::Greater),
            (Version::new(0, 5, 0), &[0, 5, 1][..], "", Ordering::Less),
            (Version::new(0, 5, 1), &[0, 5, 0][..], "", Ordering::Greater),
            (Version::new(0, 5, 0), &[0, 5][..], "", Ordering::Equal),
            (Version::new(0, 5, 1), &[0, 5][..], "", Ordering::Greater),
            (Version::new(1, 0, 0), &[1][..], "", Ordering::Equal),
            (Version::new(1, 1, 0), &[1][..], "", Ordering::Greater),
            (Version::new(1, 0, 1), &[1][..], "", Ordering::Greater),
            (
                Version::new(1, 2, 0),
                &[1, 2][..],
                "-pre",
                Ordering::Greater,
            ),
            (
                Version::new(1, 2, 1),
                &[1, 2][..],
                "-pre",
                Ordering::Greater,
            ),
            (Version::new(1, 1, 0), &[1, 2][..], "-pre", Ordering::Less),
            (
                Version::new(1, 2, 3),
                &[1, 2, 3][..],
                "-pre",
                Ordering::Greater,
            ),
            (
                Version::new(1, 2, 2),
                &[1, 2, 3][..],
                "-pre",
                Ordering::Less,
            ),
        ];
        for (left, parts, pre_release, expected) in cases {
            assert_eq!(
                compare(
                    left,
                    &Semver {
                        parts: parts.to_vec(),
                        pre_release: pre_release.into(),
                    }
                ),
                expected
            );
        }
    }

    #[test]
    fn generated_table_handles_version_holes_and_excluded_pseudo_features() {
        let unsupported_at = |parts: &[i32]| {
            unsupported_js_features(&HashMap::from([(
                Engine::Node,
                Semver {
                    parts: parts.to_vec(),
                    ..Semver::default()
                },
            )]))
        };
        assert!(unsupported_at(&[12, 19]).contains(JsFeature::DYNAMIC_IMPORT));
        assert!(!unsupported_at(&[12, 20]).contains(JsFeature::DYNAMIC_IMPORT));
        assert!(unsupported_at(&[13, 0]).contains(JsFeature::DYNAMIC_IMPORT));
        assert!(!unsupported_at(&[13, 2]).contains(JsFeature::DYNAMIC_IMPORT));
        assert!(!unsupported_at(&[0]).contains(JsFeature::INLINE_SCRIPT));
        assert_eq!(
            STRING_TO_JS_FEATURE["optional-chain"],
            JsFeature::OPTIONAL_CHAIN
        );
    }
}
