// Generated from upstream internal/compat/css_table.go.

use super::{CssFeature, CssFeatureTable, CssPrefix, Engine, PrefixData, Version, VersionRange};
use crate::internal::css_ast::Declaration;

pub(super) const CSS_TABLE: CssFeatureTable = &[
    (
        CssFeature::COLOR_FUNCTIONS,
        &[
            (Engine::Chrome, &[VersionRange::from_start(111, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(111, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(113, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(15, 4, 0)]),
            (Engine::Opera, &[VersionRange::from_start(97, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(15, 4, 0)]),
        ],
    ),
    (
        CssFeature::GRADIENT_DOUBLE_POSITION,
        &[
            (Engine::Chrome, &[VersionRange::from_start(72, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(79, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(83, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(12, 2, 0)]),
            (Engine::Opera, &[VersionRange::from_start(60, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(12, 1, 0)]),
        ],
    ),
    (
        CssFeature::GRADIENT_INTERPOLATION,
        &[
            (Engine::Chrome, &[VersionRange::from_start(111, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(111, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(137, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(16, 2, 0)]),
            (Engine::Opera, &[VersionRange::from_start(97, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(16, 2, 0)]),
        ],
    ),
    (
        CssFeature::GRADIENT_MIDPOINTS,
        &[
            (Engine::Chrome, &[VersionRange::from_start(40, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(79, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(36, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(7, 0, 0)]),
            (Engine::Opera, &[VersionRange::from_start(27, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(7, 0, 0)]),
        ],
    ),
    (
        CssFeature::HWB,
        &[
            (Engine::Chrome, &[VersionRange::from_start(101, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(101, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(96, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(15, 0, 0)]),
            (Engine::Opera, &[VersionRange::from_start(87, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(15, 0, 0)]),
        ],
    ),
    (
        CssFeature::HEX_RGBA,
        &[
            (Engine::Chrome, &[VersionRange::from_start(62, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(79, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(49, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(9, 3, 0)]),
            (Engine::Opera, &[VersionRange::from_start(49, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(10, 0, 0)]),
        ],
    ),
    (CssFeature::INLINE_STYLE, &[]),
    (
        CssFeature::INSET_PROPERTY,
        &[
            (Engine::Chrome, &[VersionRange::from_start(87, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(87, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(66, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(14, 5, 0)]),
            (Engine::Opera, &[VersionRange::from_start(73, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(14, 1, 0)]),
        ],
    ),
    (
        CssFeature::IS_PSEUDO_CLASS,
        &[
            (Engine::Chrome, &[VersionRange::from_start(88, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(88, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(78, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(14, 0, 0)]),
            (Engine::Opera, &[VersionRange::from_start(75, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(14, 0, 0)]),
        ],
    ),
    (
        CssFeature::MEDIA_RANGE,
        &[
            (Engine::Chrome, &[VersionRange::from_start(104, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(104, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(63, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(16, 4, 0)]),
            (Engine::Opera, &[VersionRange::from_start(91, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(16, 4, 0)]),
        ],
    ),
    (
        CssFeature::MODERN_RGB_HSL,
        &[
            (Engine::Chrome, &[VersionRange::from_start(66, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(79, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(52, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(12, 2, 0)]),
            (Engine::Opera, &[VersionRange::from_start(53, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(12, 1, 0)]),
        ],
    ),
    (
        CssFeature::NESTING,
        &[
            (Engine::Chrome, &[VersionRange::from_start(120, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(120, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(117, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(17, 2, 0)]),
            (Engine::Opera, &[VersionRange::from_start(106, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(17, 2, 0)]),
        ],
    ),
    (
        CssFeature::REBECCA_PURPLE,
        &[
            (Engine::Chrome, &[VersionRange::from_start(38, 0, 0)]),
            (Engine::Edge, &[VersionRange::from_start(12, 0, 0)]),
            (Engine::Firefox, &[VersionRange::from_start(33, 0, 0)]),
            (Engine::Ie, &[VersionRange::from_start(11, 0, 0)]),
            (Engine::Ios, &[VersionRange::from_start(8, 0, 0)]),
            (Engine::Opera, &[VersionRange::from_start(25, 0, 0)]),
            (Engine::Safari, &[VersionRange::from_start(9, 0, 0)]),
        ],
    ),
];

pub(super) const CSS_PREFIX_TABLE: &[(Declaration, &[PrefixData])] = &[
    (
        Declaration::Appearance,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(84, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(84, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(80, 0, 0),
                prefix: CssPrefix::MOZ,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(73, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::BackdropFilter,
        &[
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(18, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(18, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::BackgroundClip,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(15, 0, 0),
                prefix: CssPrefix::MS,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(106, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(5, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::BoxDecorationBreak,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(130, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(130, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(116, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::ClipPath,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(55, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(13, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(42, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(13, 1, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::FontKerning,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(33, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(12, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(20, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(9, 1, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::Height,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(122, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::Hyphens,
        &[
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(79, 0, 0),
                prefix: CssPrefix::MS,
            },
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(43, 0, 0),
                prefix: CssPrefix::MOZ,
            },
            PrefixData {
                engine: Engine::Ie,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::MS,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(17, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(17, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::InitialLetter,
        &[
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::Mask,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(106, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaskComposite,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(106, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaskImage,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaskOrigin,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(106, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaskPosition,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(106, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaskRepeat,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(106, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaskSize,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(120, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(106, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaxHeight,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(122, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MaxWidth,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(122, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MinHeight,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(122, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::MinWidth,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(122, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::Position,
        &[
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(13, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(13, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::PrintColorAdjust,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(15, 4, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::TabSize,
        &[
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(91, 0, 0),
                prefix: CssPrefix::MOZ,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(15, 0, 0),
                prefix: CssPrefix::O,
            },
        ],
    ),
    (
        Declaration::TextDecorationColor,
        &[
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(36, 0, 0),
                prefix: CssPrefix::MOZ,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(12, 2, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(12, 1, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::TextDecorationLine,
        &[
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(36, 0, 0),
                prefix: CssPrefix::MOZ,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(12, 2, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(12, 1, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::TextDecorationSkip,
        &[
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(12, 2, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(12, 1, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::TextEmphasisColor,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(99, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(99, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(85, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::TextEmphasisPosition,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(99, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(99, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(85, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::TextEmphasisStyle,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(99, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(99, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(85, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::TextOrientation,
        &[PrefixData {
            engine: Engine::Safari,
            without_prefix: Version::new(14, 0, 0),
            prefix: CssPrefix::WEBKIT,
        }],
    ),
    (
        Declaration::TextSizeAdjust,
        &[
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(79, 0, 0),
                prefix: CssPrefix::MS,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::UserSelect,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(54, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(79, 0, 0),
                prefix: CssPrefix::MS,
            },
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(69, 0, 0),
                prefix: CssPrefix::MOZ,
            },
            PrefixData {
                engine: Engine::Ie,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::MS,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(41, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(3, 0, 0),
                prefix: CssPrefix::KHTML,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
    (
        Declaration::Width,
        &[
            PrefixData {
                engine: Engine::Chrome,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Edge,
                without_prefix: Version::new(138, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Firefox,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Ios,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Opera,
                without_prefix: Version::new(122, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
            PrefixData {
                engine: Engine::Safari,
                without_prefix: Version::new(0, 0, 0),
                prefix: CssPrefix::WEBKIT,
            },
        ],
    ),
];
