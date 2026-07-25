// Port of upstream internal/logger/msg_ids.go.

use super::LogLevel;
use std::collections::HashMap;
use std::hash::BuildHasher;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum MsgId {
    #[default]
    None,
    JsAssertToWith,
    JsAssertTypeJson,
    JsAssignToConstant,
    JsAssignToDefine,
    JsAssignToImport,
    JsBigInt,
    JsCallImportNamespace,
    JsClassNameWillThrow,
    JsCommonJsVariableInEsm,
    JsDeleteSuperProperty,
    JsDirectEval,
    JsDuplicateCase,
    JsDuplicateClassMember,
    JsDuplicateObjectKey,
    JsEmptyImportMeta,
    JsEqualsNan,
    JsEqualsNegativeZero,
    JsEqualsNewObject,
    JsHtmlCommentInJs,
    JsImpossibleTypeof,
    JsIndirectRequire,
    JsPrivateNameWillThrow,
    JsSemicolonAfterReturn,
    JsSuspiciousBooleanNot,
    JsSuspiciousDefine,
    JsSuspiciousLogicalOperator,
    JsSuspiciousNullishCoalescing,
    JsThisIsUndefinedInEsm,
    JsUnsupportedDynamicImport,
    JsUnsupportedJsxComment,
    JsUnsupportedRegExp,
    JsUnsupportedRequireCall,
    CssSyntaxError,
    CssInvalidAtCharset,
    CssInvalidAtImport,
    CssInvalidAtLayer,
    CssInvalidCalc,
    CssJsCommentInCss,
    CssUndefinedComposesFrom,
    CssUnsupportedAtCharset,
    CssUnsupportedAtNamespace,
    CssUnsupportedProperty,
    CssUnsupportedNesting,
    BundlerAmbiguousReexport,
    BundlerDifferentPathCase,
    BundlerEmptyGlob,
    BundlerIgnoredBareImport,
    BundlerIgnoredDynamicImport,
    BundlerImportIsUndefined,
    BundlerRequireResolveNotExternal,
    SourceMapInvalidSourceMappings,
    SourceMapMissingSourceMap,
    SourceMapUnsupportedSourceMapComment,
    PackageJsonFirst,
    PackageJsonDeadCondition,
    PackageJsonInvalidBrowser,
    PackageJsonInvalidImportsOrExports,
    PackageJsonInvalidSideEffects,
    PackageJsonInvalidType,
    PackageJsonLast,
    TsConfigJsonFirst,
    TsConfigJsonCycle,
    TsConfigJsonInvalidImportsNotUsedAsValues,
    TsConfigJsonInvalidJsx,
    TsConfigJsonInvalidPaths,
    TsConfigJsonInvalidTarget,
    TsConfigJsonInvalidTopLevelOption,
    TsConfigJsonMissing,
    TsConfigJsonLast,
    End,
}

const INDIVIDUAL_IDS: &[(MsgId, &str)] = &[
    (MsgId::JsAssertToWith, "assert-to-with"),
    (MsgId::JsAssertTypeJson, "assert-type-json"),
    (MsgId::JsAssignToConstant, "assign-to-constant"),
    (MsgId::JsAssignToDefine, "assign-to-define"),
    (MsgId::JsAssignToImport, "assign-to-import"),
    (MsgId::JsBigInt, "bigint"),
    (MsgId::JsCallImportNamespace, "call-import-namespace"),
    (MsgId::JsClassNameWillThrow, "class-name-will-throw"),
    (MsgId::JsCommonJsVariableInEsm, "commonjs-variable-in-esm"),
    (MsgId::JsDeleteSuperProperty, "delete-super-property"),
    (MsgId::JsDirectEval, "direct-eval"),
    (MsgId::JsDuplicateCase, "duplicate-case"),
    (MsgId::JsDuplicateClassMember, "duplicate-class-member"),
    (MsgId::JsDuplicateObjectKey, "duplicate-object-key"),
    (MsgId::JsEmptyImportMeta, "empty-import-meta"),
    (MsgId::JsEqualsNan, "equals-nan"),
    (MsgId::JsEqualsNegativeZero, "equals-negative-zero"),
    (MsgId::JsEqualsNewObject, "equals-new-object"),
    (MsgId::JsHtmlCommentInJs, "html-comment-in-js"),
    (MsgId::JsImpossibleTypeof, "impossible-typeof"),
    (MsgId::JsIndirectRequire, "indirect-require"),
    (MsgId::JsPrivateNameWillThrow, "private-name-will-throw"),
    (MsgId::JsSemicolonAfterReturn, "semicolon-after-return"),
    (MsgId::JsSuspiciousBooleanNot, "suspicious-boolean-not"),
    (MsgId::JsSuspiciousDefine, "suspicious-define"),
    (
        MsgId::JsSuspiciousLogicalOperator,
        "suspicious-logical-operator",
    ),
    (
        MsgId::JsSuspiciousNullishCoalescing,
        "suspicious-nullish-coalescing",
    ),
    (MsgId::JsThisIsUndefinedInEsm, "this-is-undefined-in-esm"),
    (
        MsgId::JsUnsupportedDynamicImport,
        "unsupported-dynamic-import",
    ),
    (MsgId::JsUnsupportedJsxComment, "unsupported-jsx-comment"),
    (MsgId::JsUnsupportedRegExp, "unsupported-regexp"),
    (MsgId::JsUnsupportedRequireCall, "unsupported-require-call"),
    (MsgId::CssSyntaxError, "css-syntax-error"),
    (MsgId::CssInvalidAtCharset, "invalid-@charset"),
    (MsgId::CssInvalidAtImport, "invalid-@import"),
    (MsgId::CssInvalidAtLayer, "invalid-@layer"),
    (MsgId::CssInvalidCalc, "invalid-calc"),
    (MsgId::CssJsCommentInCss, "js-comment-in-css"),
    (MsgId::CssUndefinedComposesFrom, "undefined-composes-from"),
    (MsgId::CssUnsupportedAtCharset, "unsupported-@charset"),
    (MsgId::CssUnsupportedAtNamespace, "unsupported-@namespace"),
    (MsgId::CssUnsupportedProperty, "unsupported-css-property"),
    (MsgId::CssUnsupportedNesting, "unsupported-css-nesting"),
    (MsgId::BundlerAmbiguousReexport, "ambiguous-reexport"),
    (MsgId::BundlerDifferentPathCase, "different-path-case"),
    (MsgId::BundlerEmptyGlob, "empty-glob"),
    (MsgId::BundlerIgnoredBareImport, "ignored-bare-import"),
    (MsgId::BundlerIgnoredDynamicImport, "ignored-dynamic-import"),
    (MsgId::BundlerImportIsUndefined, "import-is-undefined"),
    (
        MsgId::BundlerRequireResolveNotExternal,
        "require-resolve-not-external",
    ),
    (
        MsgId::SourceMapInvalidSourceMappings,
        "invalid-source-mappings",
    ),
    (MsgId::SourceMapMissingSourceMap, "missing-source-map"),
    (
        MsgId::SourceMapUnsupportedSourceMapComment,
        "unsupported-source-map-comment",
    ),
];

const PACKAGE_JSON_IDS: &[MsgId] = &[
    MsgId::PackageJsonFirst,
    MsgId::PackageJsonDeadCondition,
    MsgId::PackageJsonInvalidBrowser,
    MsgId::PackageJsonInvalidImportsOrExports,
    MsgId::PackageJsonInvalidSideEffects,
    MsgId::PackageJsonInvalidType,
    MsgId::PackageJsonLast,
];

const TS_CONFIG_JSON_IDS: &[MsgId] = &[
    MsgId::TsConfigJsonFirst,
    MsgId::TsConfigJsonCycle,
    MsgId::TsConfigJsonInvalidImportsNotUsedAsValues,
    MsgId::TsConfigJsonInvalidJsx,
    MsgId::TsConfigJsonInvalidPaths,
    MsgId::TsConfigJsonInvalidTarget,
    MsgId::TsConfigJsonInvalidTopLevelOption,
    MsgId::TsConfigJsonMissing,
    MsgId::TsConfigJsonLast,
];

pub fn string_to_msg_ids<S: BuildHasher>(
    string: &str,
    log_level: LogLevel,
    overrides: &mut HashMap<MsgId, LogLevel, S>,
) {
    if string == "package.json" {
        overrides.extend(PACKAGE_JSON_IDS.iter().map(|id| (*id, log_level)));
    } else if string == "tsconfig.json" {
        overrides.extend(TS_CONFIG_JSON_IDS.iter().map(|id| (*id, log_level)));
    } else if let Some((id, _)) = INDIVIDUAL_IDS.iter().find(|(_, name)| *name == string) {
        overrides.insert(*id, log_level);
    }
}

#[must_use]
pub fn msg_id_to_string(id: MsgId) -> &'static str {
    if PACKAGE_JSON_IDS.contains(&id) {
        "package.json"
    } else if TS_CONFIG_JSON_IDS.contains(&id) {
        "tsconfig.json"
    } else {
        INDIVIDUAL_IDS
            .iter()
            .find_map(|(candidate, name)| (*candidate == id).then_some(*name))
            .unwrap_or("")
    }
}

/// Maps an external message ID to the largest corresponding internal ID.
#[must_use]
pub fn string_to_maximum_msg_id(string: &str) -> MsgId {
    let mut overrides = HashMap::new();
    string_to_msg_ids(string, LogLevel::Info, &mut overrides);
    overrides.keys().copied().max().unwrap_or(MsgId::None)
}

#[cfg(test)]
mod tests {
    use super::{
        INDIVIDUAL_IDS, MsgId, PACKAGE_JSON_IDS, TS_CONFIG_JSON_IDS, msg_id_to_string,
        string_to_maximum_msg_id, string_to_msg_ids,
    };
    use crate::internal::logger::LogLevel;
    use std::collections::HashMap;

    #[test]
    fn every_exposed_id_round_trips_like_upstream_test() {
        for &(id, string) in INDIVIDUAL_IDS {
            let mut overrides = HashMap::new();
            string_to_msg_ids(string, LogLevel::Error, &mut overrides);
            assert_eq!(overrides.get(&id), Some(&LogLevel::Error));
            assert_eq!(msg_id_to_string(id), string);
        }
        for (string, ids) in [
            ("package.json", PACKAGE_JSON_IDS),
            ("tsconfig.json", TS_CONFIG_JSON_IDS),
        ] {
            let mut overrides = HashMap::new();
            string_to_msg_ids(string, LogLevel::Error, &mut overrides);
            assert_eq!(overrides.len(), ids.len());
            for id in ids {
                assert_eq!(msg_id_to_string(*id), string);
            }
            assert_eq!(string_to_maximum_msg_id(string), *ids.last().unwrap());
        }
        assert_eq!(msg_id_to_string(MsgId::None), "");
        assert_eq!(msg_id_to_string(MsgId::End), "");
    }

    #[test]
    fn unknown_external_ids_are_ignored() {
        let mut overrides = HashMap::new();
        string_to_msg_ids(
            "removed-in-a-future-version",
            LogLevel::Error,
            &mut overrides,
        );
        assert!(overrides.is_empty());
    }
}
