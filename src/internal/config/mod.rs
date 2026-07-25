//! Partial port of `internal/config`.

mod known_globals;

use crate::internal::ast::Index32;
use crate::internal::js_ast::{Expr, ExprData};
use known_globals::KNOWN_GLOBALS;
use std::collections::HashMap;
use std::ops::{BitOr, BitOrAssign};
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Default)]
pub struct DefineExpr {
    pub constant: Expr,
    pub parts: Vec<String>,
    pub injected_define_index: Index32,
}

#[derive(Clone, Debug, Default)]
pub struct DefineData {
    pub key_parts: Vec<String>,
    pub define_expr: Option<DefineExpr>,
    pub flags: DefineFlags,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DefineFlags(u8);

impl DefineFlags {
    pub const NONE: Self = Self(0);
    pub const CAN_BE_REMOVED_IF_UNUSED: Self = Self(1 << 0);
    pub const CALL_CAN_BE_UNWRAPPED_IF_UNUSED: Self = Self(1 << 1);
    pub const METHOD_CALLS_MUST_BE_REPLACED_WITH_UNDEFINED: Self = Self(1 << 2);
    pub const IS_SYMBOL_INSTANCE: Self = Self(1 << 3);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for DefineFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl BitOrAssign for DefineFlags {
    fn bitor_assign(&mut self, right: Self) {
        self.0 |= right.0;
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProcessedDefines {
    pub identifier_defines: HashMap<String, DefineData>,
    pub dot_defines: HashMap<String, Vec<DefineData>>,
}

static PROCESSED_GLOBALS: OnceLock<ProcessedDefines> = OnceLock::new();

#[must_use]
pub fn process_defines(user_defines: &[DefineData]) -> ProcessedDefines {
    if user_defines.is_empty() {
        return PROCESSED_GLOBALS
            .get_or_init(|| process_defines_uncached(&[]))
            .clone();
    }
    process_defines_uncached(user_defines)
}

fn process_defines_uncached(user_defines: &[DefineData]) -> ProcessedDefines {
    let mut result = ProcessedDefines::default();
    for parts in KNOWN_GLOBALS {
        let tail = parts
            .last()
            .expect("known global paths are never empty")
            .to_string();
        if parts.len() == 1 {
            result.identifier_defines.insert(
                tail,
                DefineData {
                    flags: DefineFlags::CAN_BE_REMOVED_IF_UNUSED,
                    ..DefineData::default()
                },
            );
        } else {
            let mut flags = DefineFlags::CAN_BE_REMOVED_IF_UNUSED;
            if parts[0] == "Symbol" {
                flags |= DefineFlags::IS_SYMBOL_INSTANCE;
            }
            result
                .dot_defines
                .entry(tail)
                .or_default()
                .push(DefineData {
                    key_parts: parts.iter().map(|part| (*part).to_string()).collect(),
                    flags,
                    ..DefineData::default()
                });
        }
    }

    for (name, data) in [
        (
            "undefined",
            Expr::new(crate::internal::logger::Loc::default(), ExprData::Undefined),
        ),
        (
            "NaN",
            Expr::new(
                crate::internal::logger::Loc::default(),
                ExprData::Number(f64::NAN),
            ),
        ),
        (
            "Infinity",
            Expr::new(
                crate::internal::logger::Loc::default(),
                ExprData::Number(f64::INFINITY),
            ),
        ),
    ] {
        result.identifier_defines.insert(
            name.to_string(),
            DefineData {
                define_expr: Some(DefineExpr {
                    constant: data,
                    ..DefineExpr::default()
                }),
                ..DefineData::default()
            },
        );
    }

    for data in user_defines {
        let Some(tail) = data.key_parts.last() else {
            continue;
        };
        if data.key_parts.len() == 1 {
            let mut merged = data.clone();
            if let Some(old) = result.identifier_defines.get(tail) {
                merged.flags |= old.flags;
            }
            result.identifier_defines.insert(tail.clone(), merged);
            continue;
        }

        let definitions = result.dot_defines.entry(tail.clone()).or_default();
        if let Some(existing) = definitions
            .iter_mut()
            .find(|define| define.key_parts == data.key_parts)
        {
            let old_flags = existing.flags;
            *existing = data.clone();
            existing.flags |= old_flags;
        } else {
            definitions.push(data.clone());
        }
    }
    result
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct JsxOptions {
    pub factory: DefineExpr,
    pub fragment: DefineExpr,
    pub parse: bool,
    pub preserve: bool,
    pub automatic_runtime: bool,
    pub import_source: String,
    pub development: bool,
    pub side_effects: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum TsJsx {
    #[default]
    None,
    Preserve,
    ReactNative,
    React,
    ReactJsx,
    ReactJsxDev,
}

#[derive(Clone, Debug, Default)]
pub struct TsOptions {
    pub config: TsConfig,
    pub parse: bool,
    pub no_ambiguous_less_than: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TsConfigJsx {
    pub jsx_factory: Vec<String>,
    pub jsx_fragment_factory: Vec<String>,
    pub jsx_import_source: Option<String>,
    pub jsx: TsJsx,
}

impl TsConfigJsx {
    pub fn apply_extended_config(&mut self, base: &Self) {
        if !base.jsx_factory.is_empty() {
            self.jsx_factory.clone_from(&base.jsx_factory);
        }
        if !base.jsx_fragment_factory.is_empty() {
            self.jsx_fragment_factory
                .clone_from(&base.jsx_fragment_factory);
        }
        if base.jsx_import_source.is_some() {
            self.jsx_import_source.clone_from(&base.jsx_import_source);
        }
        if base.jsx != TsJsx::None {
            self.jsx = base.jsx;
        }
    }

    pub fn apply_to(&self, options: &mut JsxOptions) {
        match self.jsx {
            TsJsx::Preserve | TsJsx::ReactNative | TsJsx::None => {}
            TsJsx::React => {
                options.automatic_runtime = false;
                options.development = false;
            }
            TsJsx::ReactJsx => options.automatic_runtime = true,
            TsJsx::ReactJsxDev => {
                options.automatic_runtime = true;
                options.development = true;
            }
        }
        if !self.jsx_factory.is_empty() {
            options.factory = DefineExpr {
                parts: self.jsx_factory.clone(),
                ..DefineExpr::default()
            };
        }
        if !self.jsx_fragment_factory.is_empty() {
            options.fragment = DefineExpr {
                parts: self.jsx_fragment_factory.clone(),
                ..DefineExpr::default()
            };
        }
        if let Some(source) = &self.jsx_import_source {
            options.import_source.clone_from(source);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TsConfig {
    pub experimental_decorators: MaybeBool,
    pub imports_not_used_as_values: TsImportsNotUsedAsValues,
    pub preserve_value_imports: MaybeBool,
    pub target: TsTarget,
    pub use_define_for_class_fields: MaybeBool,
    pub verbatim_module_syntax: MaybeBool,
}

impl TsConfig {
    pub fn apply_extended_config(&mut self, base: Self) {
        if base.experimental_decorators != MaybeBool::Unspecified {
            self.experimental_decorators = base.experimental_decorators;
        }
        if base.imports_not_used_as_values != TsImportsNotUsedAsValues::None {
            self.imports_not_used_as_values = base.imports_not_used_as_values;
        }
        if base.preserve_value_imports != MaybeBool::Unspecified {
            self.preserve_value_imports = base.preserve_value_imports;
        }
        if base.target != TsTarget::Unspecified {
            self.target = base.target;
        }
        if base.use_define_for_class_fields != MaybeBool::Unspecified {
            self.use_define_for_class_fields = base.use_define_for_class_fields;
        }
        if base.verbatim_module_syntax != MaybeBool::Unspecified {
            self.verbatim_module_syntax = base.verbatim_module_syntax;
        }
    }

    #[must_use]
    pub fn unused_import_flags(self) -> TsUnusedImportFlags {
        if self.verbatim_module_syntax == MaybeBool::True {
            return TsUnusedImportFlags::KEEP_STMT | TsUnusedImportFlags::KEEP_VALUES;
        }
        let mut flags = TsUnusedImportFlags::NONE;
        if self.preserve_value_imports == MaybeBool::True {
            flags |= TsUnusedImportFlags::KEEP_VALUES;
        }
        if matches!(
            self.imports_not_used_as_values,
            TsImportsNotUsedAsValues::Preserve | TsImportsNotUsedAsValues::Error
        ) {
            flags |= TsUnusedImportFlags::KEEP_STMT;
        }
        flags
    }
}

macro_rules! simple_enum {
    ($name:ident { $default:ident $(, $variant:ident)* $(,)? }) => {
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        #[repr(u8)]
        pub enum $name {
            #[default]
            $default,
            $($variant),*
        }
    };
}

simple_enum!(Platform {
    Browser,
    Node,
    Neutral
});
simple_enum!(SourceMap {
    None,
    Inline,
    LinkedWithComment,
    ExternalWithoutComment,
    InlineAndExternal
});
simple_enum!(LegalComments {
    Inline,
    None,
    EndOfFile,
    LinkedWithComment,
    ExternalWithoutComment
});

impl LegalComments {
    #[must_use]
    pub const fn has_external_file(self) -> bool {
        matches!(self, Self::LinkedWithComment | Self::ExternalWithoutComment)
    }
}

simple_enum!(Loader {
    None,
    Base64,
    Binary,
    Copy,
    Css,
    DataUrl,
    Default,
    Empty,
    File,
    GlobalCss,
    Js,
    Json,
    WithTypeJson,
    Jsx,
    LocalCss,
    Text,
    Ts,
    TsNoAmbiguousLessThan,
    Tsx
});

pub const LOADER_TO_STRING: &[&str] = &[
    "none",
    "base64",
    "binary",
    "copy",
    "css",
    "dataurl",
    "default",
    "empty",
    "file",
    "global-css",
    "js",
    "json",
    "json",
    "jsx",
    "local-css",
    "text",
    "ts",
    "ts",
    "tsx",
];

impl Loader {
    #[must_use]
    pub const fn is_type_script(self) -> bool {
        matches!(self, Self::Ts | Self::TsNoAmbiguousLessThan | Self::Tsx)
    }

    #[must_use]
    pub const fn is_css(self) -> bool {
        matches!(self, Self::Css | Self::GlobalCss | Self::LocalCss)
    }

    #[must_use]
    pub const fn can_have_source_map(self) -> bool {
        matches!(
            self,
            Self::Js
                | Self::Jsx
                | Self::Ts
                | Self::TsNoAmbiguousLessThan
                | Self::Tsx
                | Self::Css
                | Self::GlobalCss
                | Self::LocalCss
                | Self::Json
                | Self::WithTypeJson
                | Self::Text
        )
    }
}

#[must_use]
pub fn loader_from_file_extension<S: std::hash::BuildHasher>(
    extension_to_loader: &HashMap<String, Loader, S>,
    mut base: &str,
) -> Loader {
    if let Some(mut index) = base.find('.') {
        loop {
            if let Some(&loader) = extension_to_loader.get(&base[index..]) {
                return loader;
            }
            base = &base[index + 1..];
            let Some(next) = base.find('.') else {
                break;
            };
            index = next;
        }
    } else if let Some(&loader) = extension_to_loader.get("") {
        return loader;
    }
    Loader::None
}

simple_enum!(Format {
    Preserve,
    Iife,
    CommonJs,
    EsModule
});

impl Format {
    #[must_use]
    pub const fn keep_esm_import_export_syntax(self) -> bool {
        matches!(self, Self::Preserve | Self::EsModule)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Iife => "iife",
            Self::CommonJs => "cjs",
            Self::EsModule => "esm",
            Self::Preserve => "",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StdinInfo {
    pub contents: String,
    pub source_file: String,
    pub abs_resolve_dir: String,
    pub loader: Loader,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WildcardPattern {
    pub prefix: String,
    pub suffix: String,
}

#[derive(Clone, Debug, Default)]
pub struct ExternalMatchers {
    pub exact: HashMap<String, bool>,
    pub patterns: Vec<WildcardPattern>,
}

impl ExternalMatchers {
    #[must_use]
    pub fn has_matchers(&self) -> bool {
        !self.exact.is_empty() || !self.patterns.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExternalSettings {
    pub pre_resolve: ExternalMatchers,
    pub post_resolve: ExternalMatchers,
}

simple_enum!(ApiCall { Build, Transform });
simple_enum!(Mode {
    PassThrough,
    ConvertFormat,
    Bundle
});
simple_enum!(MaybeBool {
    Unspecified,
    True,
    False
});
simple_enum!(TsImportsNotUsedAsValues {
    None,
    Remove,
    Preserve,
    Error
});
simple_enum!(TsTarget {
    Unspecified,
    BelowEs2022,
    AtOrAboveEs2022
});

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TsUnusedImportFlags(u8);

impl TsUnusedImportFlags {
    pub const NONE: Self = Self(0);
    pub const KEEP_STMT: Self = Self(1 << 0);
    pub const KEEP_VALUES: Self = Self(1 << 1);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for TsUnusedImportFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl BitOrAssign for TsUnusedImportFlags {
    fn bitor_assign(&mut self, right: Self) {
        self.0 |= right.0;
    }
}

#[derive(Debug, Default)]
pub struct CancelFlag(AtomicBool);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn did_cancel(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

simple_enum!(PathPlaceholder {
    None,
    Dir,
    Name,
    Hash,
    Ext
});

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathTemplate {
    pub data: String,
    pub placeholder: PathPlaceholder,
}

#[derive(Clone, Debug, Default)]
pub struct PathPlaceholders {
    pub dir: Option<String>,
    pub name: Option<String>,
    pub hash: Option<String>,
    pub ext: Option<String>,
}

impl PathPlaceholders {
    #[must_use]
    pub fn get(&self, placeholder: PathPlaceholder) -> Option<&str> {
        match placeholder {
            PathPlaceholder::Dir => self.dir.as_deref(),
            PathPlaceholder::Name => self.name.as_deref(),
            PathPlaceholder::Hash => self.hash.as_deref(),
            PathPlaceholder::Ext => self.ext.as_deref(),
            PathPlaceholder::None => None,
        }
    }
}

#[must_use]
pub fn template_to_string(template: &[PathTemplate]) -> String {
    let mut result = String::new();
    for part in template {
        result.push_str(&part.data);
        result.push_str(match part.placeholder {
            PathPlaceholder::Dir => "[dir]",
            PathPlaceholder::Name => "[name]",
            PathPlaceholder::Hash => "[hash]",
            PathPlaceholder::Ext => "[ext]",
            PathPlaceholder::None => "",
        });
    }
    result
}

#[must_use]
pub fn has_placeholder(template: &[PathTemplate], placeholder: PathPlaceholder) -> bool {
    template.iter().any(|part| part.placeholder == placeholder)
}

#[must_use]
pub fn substitute_template(
    template: &[PathTemplate],
    placeholders: &PathPlaceholders,
) -> Vec<PathTemplate> {
    let should_substitute = template.iter().enumerate().any(|(index, part)| {
        placeholders.get(part.placeholder).is_some()
            || (part.placeholder == PathPlaceholder::None && index + 1 < template.len())
    });
    if !should_substitute {
        return template.to_vec();
    }

    let mut result: Vec<PathTemplate> = Vec::with_capacity(template.len());
    for original in template {
        let mut part = original.clone();
        if let Some(substitution) = placeholders.get(part.placeholder) {
            part.data.push_str(substitution);
            part.placeholder = PathPlaceholder::None;
        }
        if let Some(last) = result.last_mut()
            && last.placeholder == PathPlaceholder::None
        {
            last.data.push_str(&part.data);
            last.placeholder = part.placeholder;
        } else {
            result.push(part);
        }
    }
    result
}

#[must_use]
pub const fn should_call_runtime_require(mode: Mode, output_format: Format) -> bool {
    matches!(mode, Mode::Bundle) && !matches!(output_format, Format::CommonJs)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TsAlwaysStrict {
    pub name: String,
    pub source: crate::internal::logger::Source,
    pub range: crate::internal::logger::Range,
    pub value: bool,
}

#[must_use]
pub fn pretty_print_target_environment(
    original_target_environment: &str,
    unsupported_js_feature_overrides_mask: crate::internal::compat::JsFeature,
) -> String {
    let mut result = "the configured target environment".to_string();
    let count = unsupported_js_feature_overrides_mask.count();
    let overrides = if count == 0 {
        String::new()
    } else if count == 1 {
        " + 1 override".to_string()
    } else {
        format!(" + {count} overrides")
    };
    if !original_target_environment.is_empty() {
        result.push_str(" (");
        result.push_str(original_target_environment);
        result.push_str(&overrides);
        result.push(')');
    }
    result
}

simple_enum!(MetafileFormat {
    Unminified,
    Minified
});

impl MetafileFormat {
    #[must_use]
    pub fn maybe_remove_whitespace(self, text: &str) -> String {
        if self == Self::Minified {
            text.chars()
                .filter(|character| !matches!(character, ' ' | '\n'))
                .collect()
        } else {
            text.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DefineData, DefineExpr, DefineFlags, Format, JsxOptions, Loader, MaybeBool, MetafileFormat,
        PathPlaceholder, PathPlaceholders, PathTemplate, TsConfig, TsConfigJsx,
        TsImportsNotUsedAsValues, TsJsx, TsUnusedImportFlags, has_placeholder,
        loader_from_file_extension, pretty_print_target_environment, process_defines,
        should_call_runtime_require, substitute_template, template_to_string,
    };
    use std::collections::HashMap;

    #[test]
    fn tsconfig_inheritance_and_jsx_application_match_upstream() {
        let mut derived = TsConfigJsx::default();
        derived.apply_extended_config(&TsConfigJsx {
            jsx_factory: vec!["h".into()],
            jsx_import_source: Some("preact".into()),
            jsx: TsJsx::ReactJsxDev,
            ..TsConfigJsx::default()
        });
        let mut jsx = JsxOptions::default();
        derived.apply_to(&mut jsx);
        assert_eq!(jsx.factory.parts, ["h"]);
        assert_eq!(jsx.import_source, "preact");
        assert!(jsx.automatic_runtime);
        assert!(jsx.development);
    }

    #[test]
    fn unused_import_flags_combine_legacy_ts_options() {
        let config = TsConfig {
            imports_not_used_as_values: TsImportsNotUsedAsValues::Preserve,
            preserve_value_imports: MaybeBool::True,
            ..TsConfig::default()
        };
        let flags = config.unused_import_flags();
        assert!(flags.contains(TsUnusedImportFlags::KEEP_STMT));
        assert!(flags.contains(TsUnusedImportFlags::KEEP_VALUES));
    }

    #[test]
    fn loader_uses_longest_matching_extension() {
        let loaders = HashMap::from([
            (".css".into(), Loader::Css),
            (".module.css".into(), Loader::LocalCss),
        ]);
        assert_eq!(
            loader_from_file_extension(&loaders, "app.module.css"),
            Loader::LocalCss
        );
        assert_eq!(loader_from_file_extension(&loaders, "app.css"), Loader::Css);
    }

    #[test]
    fn path_templates_substitute_and_merge_literal_parts() {
        let template = vec![
            PathTemplate {
                data: "out/".into(),
                placeholder: PathPlaceholder::Dir,
            },
            PathTemplate {
                data: "-".into(),
                placeholder: PathPlaceholder::Name,
            },
        ];
        assert_eq!(template_to_string(&template), "out/[dir]-[name]");
        assert!(has_placeholder(&template, PathPlaceholder::Name));
        let substituted = substitute_template(
            &template,
            &PathPlaceholders {
                dir: Some("src".into()),
                name: Some("entry".into()),
                ..PathPlaceholders::default()
            },
        );
        assert_eq!(substituted.len(), 1);
        assert_eq!(substituted[0].data, "out/src-entry");
        assert!(should_call_runtime_require(
            super::Mode::Bundle,
            Format::EsModule
        ));
    }

    #[test]
    fn known_globals_and_primitive_defines_are_processed() {
        let defines = process_defines(&[]);
        assert!(
            defines.identifier_defines["window"]
                .flags
                .contains(DefineFlags::CAN_BE_REMOVED_IF_UNUSED)
        );
        assert!(
            defines.dot_defines["assign"]
                .iter()
                .any(|define| define.key_parts == ["Object", "assign"])
        );
        assert!(defines.dot_defines["iterator"].iter().any(|define| {
            define.key_parts == ["Symbol", "iterator"]
                && define.flags.contains(DefineFlags::IS_SYMBOL_INSTANCE)
        }));
        assert!(matches!(
            defines.identifier_defines["undefined"]
                .define_expr
                .as_ref()
                .and_then(|define| define.constant.data.as_deref()),
            Some(crate::internal::js_ast::ExprData::Undefined)
        ));
    }

    #[test]
    fn user_defines_override_values_and_merge_flags() {
        let defines = process_defines(&[
            DefineData {
                key_parts: vec!["Object".into(), "assign".into()],
                define_expr: Some(DefineExpr {
                    parts: vec!["replacement".into()],
                    ..DefineExpr::default()
                }),
                flags: DefineFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED,
            },
            DefineData {
                key_parts: vec!["window".into()],
                flags: DefineFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED,
                ..DefineData::default()
            },
        ]);
        let object_assign = defines.dot_defines["assign"]
            .iter()
            .find(|define| define.key_parts == ["Object", "assign"])
            .expect("Object.assign define");
        assert_eq!(
            object_assign
                .define_expr
                .as_ref()
                .expect("replacement")
                .parts,
            ["replacement"]
        );
        assert!(
            object_assign
                .flags
                .contains(DefineFlags::CAN_BE_REMOVED_IF_UNUSED)
        );
        assert!(
            object_assign
                .flags
                .contains(DefineFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED)
        );
        assert!(
            defines.identifier_defines["window"]
                .flags
                .contains(DefineFlags::CAN_BE_REMOVED_IF_UNUSED)
        );
    }

    #[test]
    fn target_environment_and_metafile_formatting_match_upstream() {
        let overrides = crate::internal::compat::JsFeature::ARROW
            | crate::internal::compat::JsFeature::ASYNC_AWAIT;
        assert_eq!(
            pretty_print_target_environment("chrome100", overrides),
            "the configured target environment (chrome100 + 2 overrides)"
        );
        assert_eq!(
            pretty_print_target_environment("", overrides),
            "the configured target environment"
        );
        assert_eq!(
            MetafileFormat::Minified.maybe_remove_whitespace("{\n \"x\": 1\n}"),
            "{\"x\":1}"
        );
    }
}
