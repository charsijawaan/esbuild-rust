//! Port of upstream `internal/resolver`.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::internal::{
    config::{
        MaybeBool, TsAlwaysStrict, TsConfig, TsConfigJsx, TsImportsNotUsedAsValues, TsJsx, TsTarget,
    },
    fs::{DifferentCase, Fs},
    helpers::{is_inside_node_modules, utf16_to_string},
    js_ast::{Expr, ExprData, ModuleTypeData},
    js_lexer::{JsonFlavor, range_of_identifier},
    js_parser::{JsonOptions, parse_json},
    logger::{LineColumnTracker, Loc, Log, Msg, MsgData, MsgId, MsgKind, Path, Range, Source},
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathPair {
    pub primary: Path,
    pub secondary: Path,
    pub is_external: bool,
}

impl PathPair {
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Path> {
        let has_secondary = self.has_secondary();
        [&mut self.primary, &mut self.secondary]
            .into_iter()
            .take(if has_secondary { 2 } else { 1 })
    }

    #[must_use]
    pub fn has_secondary(&self) -> bool {
        !self.secondary.text.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SideEffectsData {
    pub source: Option<Source>,
    pub plugin_name: String,
    pub range: Range,
    pub is_side_effects_array_in_json: bool,
}

#[derive(Clone, Default)]
pub struct ResolveResult {
    pub path_pair: PathPair,
    pub plugin_data: Option<Arc<dyn Any + Send + Sync>>,
    pub different_case: Option<DifferentCase>,
    pub primary_side_effects_data: Option<SideEffectsData>,
    pub ts_config_jsx: TsConfigJsx,
    pub ts_config: Option<TsConfig>,
    pub ts_always_strict: Option<TsAlwaysStrict>,
    pub module_type_data: ModuleTypeData,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SuggestionRange {
    #[default]
    Full,
    End,
}

#[derive(Clone, Default)]
pub struct DebugMeta {
    notes: Vec<MsgData>,
    suggestion_text: String,
    suggestion_message: String,
    suggestion_range: SuggestionRange,
    pub modified_import_path: String,
}

impl DebugMeta {
    pub fn log_error_msg(
        mut self,
        log: &Log,
        source: Option<&Source>,
        range: Range,
        text: impl Into<String>,
        suggestion: &str,
        notes: &[MsgData],
    ) {
        let mut tracker = LineColumnTracker::new(source);
        if source.is_some() && !self.suggestion_message.is_empty() {
            let suggestion_range = if self.suggestion_range == SuggestionRange::End {
                Range {
                    loc: Loc {
                        start: range.end() - 1,
                    },
                    ..Range::default()
                }
            } else {
                range
            };
            let mut data = tracker.msg_data(suggestion_range, self.suggestion_message);
            if let Some(location) = &mut data.location {
                location.suggestion = self.suggestion_text;
            }
            self.notes.push(data);
        }

        let mut data = tracker.msg_data(range, text);
        if !suggestion.is_empty()
            && let Some(location) = &mut data.location
        {
            location.suggestion = suggestion.to_string();
        }
        self.notes.extend_from_slice(notes);
        log.add_msg(Msg {
            notes: self.notes,
            data,
            kind: MsgKind::Error,
            ..Msg::new(MsgKind::Error, "")
        });
    }
}

#[derive(Clone, Debug, Default)]
pub struct TsConfigJson {
    pub abs_path: String,
    pub base_url: Option<String>,
    pub base_url_for_paths: String,
    pub paths: Option<TsConfigPaths>,
    ts_target_key: TsTargetKey,
    pub ts_strict: Option<TsAlwaysStrict>,
    pub ts_always_strict: Option<TsAlwaysStrict>,
    pub jsx_settings: TsConfigJsx,
    pub settings: TsConfig,
}

impl TsConfigJson {
    pub fn apply_extended_config(&mut self, base: &Self) {
        if base.ts_target_key.range.len > 0 {
            self.ts_target_key.clone_from(&base.ts_target_key);
        }
        if base.ts_strict.is_some() {
            self.ts_strict.clone_from(&base.ts_strict);
        }
        if base.ts_always_strict.is_some() {
            self.ts_always_strict.clone_from(&base.ts_always_strict);
        }
        if base.base_url.is_some() {
            self.base_url.clone_from(&base.base_url);
        }
        if base.paths.is_some() {
            self.paths.clone_from(&base.paths);
            self.base_url_for_paths.clone_from(&base.base_url_for_paths);
        }
        self.jsx_settings.apply_extended_config(&base.jsx_settings);
        self.settings.apply_extended_config(base.settings);
    }

    #[must_use]
    pub fn ts_always_strict_or_strict(&self) -> Option<&TsAlwaysStrict> {
        self.ts_always_strict.as_ref().or(self.ts_strict.as_ref())
    }
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct TsTargetKey {
    lower_value: String,
    source: Source,
    range: Range,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TsConfigPath {
    pub text: String,
    pub loc: Loc,
}

#[derive(Clone, Debug, Default)]
pub struct TsConfigPaths {
    pub map: HashMap<String, Vec<TsConfigPath>>,
    pub source: Source,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TsConfigPathCandidate {
    pub path: String,
    pub loc: Loc,
}

/// Return the ordered file-system candidates selected by TypeScript's `paths`
/// matching algorithm. `Some` means a mapping matched, even if all candidates
/// were ignored because they ended in `.d.ts`.
#[must_use]
pub fn match_tsconfig_path_candidates(
    config: &TsConfigJson,
    import_path: &str,
    file_system: &dyn Fs,
) -> Option<Vec<TsConfigPathCandidate>> {
    let paths = config.paths.as_ref()?;
    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or(&config.base_url_for_paths);

    if let Some(substitutions) = paths.map.get(import_path) {
        return Some(resolve_tsconfig_substitutions(
            substitutions,
            None,
            base_url,
            file_system,
        ));
    }

    let mut longest_match: Option<(&str, &str, &[TsConfigPath])> = None;
    for (key, substitutions) in &paths.map {
        let Some(star_index) = key.find('*') else {
            continue;
        };
        let prefix = &key[..star_index];
        let suffix = &key[star_index + 1..];
        if !import_path.starts_with(prefix) || !import_path.ends_with(suffix) {
            continue;
        }
        let is_better = longest_match.is_none_or(|(old_prefix, old_suffix, _)| {
            prefix.len() > old_prefix.len()
                || (prefix.len() == old_prefix.len() && suffix.len() > old_suffix.len())
        });
        if is_better {
            longest_match = Some((prefix, suffix, substitutions));
        }
    }

    longest_match.map(|(prefix, suffix, substitutions)| {
        let matched = &import_path[prefix.len()..import_path.len() - suffix.len()];
        resolve_tsconfig_substitutions(substitutions, Some(matched), base_url, file_system)
    })
}

fn resolve_tsconfig_substitutions(
    substitutions: &[TsConfigPath],
    matched: Option<&str>,
    base_url: &str,
    file_system: &dyn Fs,
) -> Vec<TsConfigPathCandidate> {
    substitutions
        .iter()
        .filter_map(|substitution| {
            let path = matched.map_or_else(
                || substitution.text.clone(),
                |matched| substitution.text.replacen('*', matched, 1),
            );
            if has_case_insensitive_suffix(&path, ".d.ts") {
                return None;
            }
            Some(TsConfigPathCandidate {
                path: if file_system.is_abs(&path) {
                    path
                } else {
                    file_system.join(&[base_url, &path])
                },
                loc: substitution.loc,
            })
        })
        .collect()
}

fn has_case_insensitive_suffix(text: &str, suffix: &str) -> bool {
    text.get(text.len().saturating_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

#[must_use]
pub fn parse_bare_identifier(specifier: &str) -> Option<(&str, &str)> {
    let first_slash = specifier.find('/');
    let identifier = if specifier.starts_with('@') {
        let first_slash = first_slash?;
        specifier[first_slash + 1..]
            .find('/')
            .map_or(specifier, |second_slash| {
                &specifier[..first_slash + 1 + second_slash]
            })
    } else {
        first_slash.map_or(specifier, |slash| &specifier[..slash])
    };
    Some((identifier, &specifier[identifier.len()..]))
}

#[must_use]
pub fn parse_esm_package_name(specifier: &str) -> Option<(&str, String)> {
    if specifier.is_empty() {
        return None;
    }
    let first_slash = specifier.find('/');
    let package_name = if specifier.starts_with('@') {
        let first_slash = first_slash?;
        specifier[first_slash + 1..]
            .find('/')
            .map_or(specifier, |second_slash| {
                &specifier[..first_slash + 1 + second_slash]
            })
    } else {
        first_slash.map_or(specifier, |slash| &specifier[..slash])
    };
    if package_name.starts_with('.') || package_name.contains(['\\', '%']) {
        return None;
    }
    Some((
        package_name,
        format!(".{}", &specifier[package_name.len()..]),
    ))
}

#[must_use]
pub fn find_invalid_package_segment(path: &str) -> Option<&str> {
    path.split(['/', '\\'])
        .skip(1)
        .find(|segment| matches!(*segment, "." | ".." | "node_modules"))
}

#[must_use]
pub fn globstar_to_escaped_regexp(glob: &str) -> (String, bool) {
    let bytes = glob.as_bytes();
    let mut result = Vec::with_capacity(bytes.len() + 2);
    result.push(b'^');
    let mut had_wildcard = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\\' | b'^' | b'$' | b'.' | b'+' | b'|' | b'(' | b')' | b'[' | b']' | b'{' | b'}' => {
                result.push(b'\\');
                result.push(byte);
            }
            b'?' => {
                result.push(b'.');
                had_wildcard = true;
            }
            b'*' => {
                let previous = index.checked_sub(1).and_then(|i| bytes.get(i)).copied();
                let mut star_count = 1;
                while bytes.get(index + 1) == Some(&b'*') {
                    star_count += 1;
                    index += 1;
                }
                let next = bytes.get(index + 1).copied();
                let is_globstar = star_count > 1
                    && previous.is_none_or(|byte| byte == b'/')
                    && next.is_none_or(|byte| byte == b'/');
                if is_globstar {
                    result.extend_from_slice(b"(?:[^/]*(?:/|$))*");
                    if next == Some(b'/') {
                        index += 1;
                    }
                } else {
                    result.extend_from_slice(b"[^/]*");
                }
                had_wildcard = true;
            }
            _ => result.push(byte),
        }
        index += 1;
    }
    result.push(b'$');
    (String::from_utf8_lossy(&result).into_owned(), had_wildcard)
}

#[must_use]
pub fn sort_package_expansion_keys<'a>(keys: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort_by(|left, right| package_expansion_key_cmp(left, right));
    keys
}

fn package_expansion_key_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_star = left.find('*');
    let right_star = right.find('*');
    let left_base_length = left_star.unwrap_or(left.len());
    let right_base_length = right_star.unwrap_or(right.len());
    right_base_length
        .cmp(&left_base_length)
        .then_with(|| match (left_star, right_star) {
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            _ => right.len().cmp(&left.len()),
        })
}

#[derive(Clone, Debug)]
pub struct PackageMap {
    pub root: PackageMapEntry,
    pub property_key: String,
    pub property_key_loc: Loc,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PackageMapKind {
    #[default]
    Null,
    String,
    Array,
    Object,
    Invalid,
}

#[derive(Clone, Debug, Default)]
pub struct PackageMapEntry {
    pub string: String,
    pub array: Vec<PackageMapEntry>,
    pub map: Vec<PackageMapProperty>,
    pub expansion_keys: Vec<PackageMapProperty>,
    pub first_token: Range,
    pub kind: PackageMapKind,
}

impl PackageMapEntry {
    #[must_use]
    pub fn value_for_key(&self, key: &str) -> Option<&Self> {
        self.map
            .iter()
            .find(|property| property.key == key)
            .map(|property| &property.value)
    }

    #[must_use]
    pub fn keys_start_with_dot(&self) -> bool {
        self.map
            .first()
            .is_some_and(|property| property.key.starts_with('.'))
    }
}

#[derive(Clone, Debug, Default)]
pub struct PackageMapProperty {
    pub key: String,
    pub value: PackageMapEntry,
    pub key_range: Range,
}

#[must_use]
pub fn parse_imports_exports_map(
    source: &Source,
    log: &Log,
    json: &Expr,
    property_key: &str,
    property_key_loc: Loc,
) -> Option<PackageMap> {
    let mut tracker = LineColumnTracker::new(Some(source));
    let root = visit_package_map_entry(source, log, &mut tracker, json);
    if root.kind == PackageMapKind::Null {
        return None;
    }
    Some(PackageMap {
        root,
        property_key: property_key.to_string(),
        property_key_loc,
    })
}

#[allow(clippy::too_many_lines)]
fn visit_package_map_entry(
    source: &Source,
    log: &Log,
    tracker: &mut LineColumnTracker,
    expression: &Expr,
) -> PackageMapEntry {
    match expression.data.as_deref() {
        Some(ExprData::Null) => PackageMapEntry {
            kind: PackageMapKind::Null,
            first_token: range_of_identifier(source, expression.loc),
            ..PackageMapEntry::default()
        },
        Some(ExprData::String(string)) => PackageMapEntry {
            kind: PackageMapKind::String,
            first_token: source.range_of_string(expression.loc),
            string: String::from_utf8_lossy(&utf16_to_string(&string.value)).into_owned(),
            ..PackageMapEntry::default()
        },
        Some(ExprData::Array(array)) => PackageMapEntry {
            kind: PackageMapKind::Array,
            first_token: Range {
                loc: expression.loc,
                len: 1,
            },
            array: array
                .items
                .iter()
                .map(|item| visit_package_map_entry(source, log, tracker, item))
                .collect(),
            ..PackageMapEntry::default()
        },
        Some(ExprData::Object(object)) => {
            let first_token = Range {
                loc: expression.loc,
                len: 1,
            };
            let mut map: Vec<PackageMapProperty> = Vec::with_capacity(object.properties.len());
            let mut expansion_keys = Vec::new();
            let mut is_conditional_sugar = None;
            let mut found_default: Option<Range> = None;
            let mut found_import: Option<Range> = None;
            let mut found_require: Option<Range> = None;
            let mut dead_ranges = Vec::new();
            let mut dead_reason = "";
            let mut dead_notes = Vec::new();

            for property in &object.properties {
                let key = get_string(&property.key).unwrap_or_default();
                let key_range = source.range_of_string(property.key.loc);
                let current_is_conditional_sugar = !key.starts_with('.');
                if let Some(previous_kind) = is_conditional_sugar
                    && previous_kind != current_is_conditional_sugar
                {
                    let previous = map.last().expect("mixed object has a previous key");
                    let note = tracker.msg_data(
                        previous.key_range,
                        format!(
                            "The key {key:?} is incompatible with the previous key {:?}:",
                            previous.key
                        ),
                    );
                    log.add_id_with_notes(
                        MsgId::PackageJsonInvalidImportsOrExports,
                        MsgKind::Warning,
                        Some(tracker),
                        key_range,
                        "This object cannot contain keys that both start with \".\" and don't start with \".\"",
                        vec![note],
                    );
                    return PackageMapEntry {
                        kind: PackageMapKind::Invalid,
                        first_token,
                        ..PackageMapEntry::default()
                    };
                }
                is_conditional_sugar = Some(current_is_conditional_sugar);

                if found_default.is_some() || (found_import.is_some() && found_require.is_some()) {
                    dead_ranges.push(key_range);
                    if dead_reason.is_empty() && key != "default" {
                        if let Some(range) = found_default {
                            dead_reason = "\"default\"";
                            dead_notes = vec![tracker.msg_data(
                                range,
                                "The \"default\" condition comes earlier and will always be chosen:",
                            )];
                        } else {
                            dead_reason = "both \"import\" and \"require\"";
                            dead_notes = vec![
                                tracker.msg_data(
                                    found_import.expect("import condition"),
                                    "The \"import\" condition comes earlier and will be used for all \"import\" statements:",
                                ),
                                tracker.msg_data(
                                    found_require.expect("require condition"),
                                    "The \"require\" condition comes earlier and will be used for all \"require\" calls:",
                                ),
                            ];
                        }
                    }
                } else {
                    match key.as_str() {
                        "default" => found_default = Some(key_range),
                        "import" => found_import = Some(key_range),
                        "require" => found_require = Some(key_range),
                        _ => {}
                    }
                }

                let entry = PackageMapProperty {
                    key: key.clone(),
                    value: visit_package_map_entry(source, log, tracker, &property.value_or_nil),
                    key_range,
                };
                if key.ends_with('/') || key.contains('*') {
                    expansion_keys.push(entry.clone());
                }
                map.push(entry);
            }

            expansion_keys.sort_by(|left, right| package_expansion_key_cmp(&left.key, &right.key));
            if !dead_reason.is_empty() {
                let kind = if is_inside_node_modules(&source.key_path.text) {
                    MsgKind::Debug
                } else {
                    MsgKind::Warning
                };
                let conditions = dead_ranges
                    .iter()
                    .map(|range| String::from_utf8_lossy(source.text_for_range(*range)))
                    .collect::<Vec<_>>()
                    .join(" and ");
                let (condition_word, comes_word) = if dead_ranges.len() > 1 {
                    ("conditions", "they come")
                } else {
                    ("condition", "it comes")
                };
                log.add_id_with_notes(
                    MsgId::PackageJsonDeadCondition,
                    kind,
                    Some(tracker),
                    dead_ranges[0],
                    format!(
                        "The {condition_word} {conditions} here will never be used as {comes_word} after {dead_reason}"
                    ),
                    dead_notes,
                );
            }

            PackageMapEntry {
                kind: PackageMapKind::Object,
                first_token,
                map,
                expansion_keys,
                ..PackageMapEntry::default()
            }
        }
        data => {
            let first_token = match data {
                Some(ExprData::Boolean(_)) => range_of_identifier(source, expression.loc),
                Some(ExprData::Number(_)) => source.range_of_number(expression.loc),
                _ => Range {
                    loc: expression.loc,
                    ..Range::default()
                },
            };
            log.add_id(
                MsgId::PackageJsonInvalidImportsOrExports,
                MsgKind::Warning,
                Some(tracker),
                first_token,
                "This value must be a string, an object, an array, or null",
            );
            PackageMapEntry {
                kind: PackageMapKind::Invalid,
                first_token,
                ..PackageMapEntry::default()
            }
        }
    }
}

type ExtendsCallback<'a> = dyn FnMut(&str, Range) -> Option<TsConfigJson> + 'a;

#[allow(clippy::too_many_lines)]
pub fn parse_tsconfig_json(
    log: &Log,
    source: &Source,
    file_system: &dyn Fs,
    file_dir: &str,
    config_dir: &str,
    mut extends: Option<&mut ExtendsCallback<'_>>,
) -> Option<TsConfigJson> {
    let (json, ok) = parse_json(
        log.clone(),
        source.clone(),
        JsonOptions {
            flavor: JsonFlavor::TsConfigJson,
            ..JsonOptions::default()
        },
    );
    if !ok {
        return None;
    }

    let mut result = TsConfigJson {
        abs_path: source.key_path.text.clone(),
        ..TsConfigJson::default()
    };
    let mut tracker = LineColumnTracker::new(Some(source));

    if let Some(callback) = extends.as_mut()
        && let Some((value, _)) = get_property(&json, "extends")
    {
        match value.data.as_deref() {
            Some(ExprData::String(_)) => {
                if let Some(text) = get_string(value)
                    && let Some(base) = callback(&text, source.range_of_string(value.loc))
                {
                    result.apply_extended_config(&base);
                }
            }
            Some(ExprData::Array(array)) => {
                for item in &array.items {
                    if let Some(text) = get_string(item)
                        && let Some(base) = callback(&text, source.range_of_string(item.loc))
                    {
                        result.apply_extended_config(&base);
                    }
                }
            }
            _ => {}
        }
    }

    if let Some((compiler_options, _)) = get_property(&json, "compilerOptions") {
        if let Some((value, _)) = get_property(compiler_options, "baseUrl")
            && let Some(mut text) = get_string(value)
        {
            text = substituted_path_with_config_dir_template(file_system, &text, config_dir);
            if !file_system.is_abs(&text) {
                text = file_system.join(&[file_dir, &text]);
            }
            result.base_url = Some(text);
        }

        if let Some((value, _)) = get_property(compiler_options, "jsx")
            && let Some(text) = get_string(value)
        {
            result.jsx_settings.jsx = match text.to_lowercase().as_str() {
                "preserve" => TsJsx::Preserve,
                "react-native" => TsJsx::ReactNative,
                "react" => TsJsx::React,
                "react-jsx" => TsJsx::ReactJsx,
                "react-jsxdev" => TsJsx::ReactJsxDev,
                _ => result.jsx_settings.jsx,
            };
        }

        if let Some((value, _)) = get_property(compiler_options, "jsxFactory")
            && let Some(text) = get_string(value)
        {
            result.jsx_settings.jsx_factory =
                parse_member_expression_for_jsx(log, source, &mut tracker, value.loc, &text);
        }
        if let Some((value, _)) = get_property(compiler_options, "jsxFragmentFactory")
            && let Some(text) = get_string(value)
        {
            result.jsx_settings.jsx_fragment_factory =
                parse_member_expression_for_jsx(log, source, &mut tracker, value.loc, &text);
        }
        if let Some((value, _)) = get_property(compiler_options, "jsxImportSource")
            && let Some(text) = get_string(value)
        {
            result.jsx_settings.jsx_import_source = Some(text);
        }

        if let Some((value, _)) = get_property(compiler_options, "experimentalDecorators")
            && let Some(boolean) = get_bool(value)
        {
            result.settings.experimental_decorators = maybe_bool(boolean);
        }
        if let Some((value, _)) = get_property(compiler_options, "useDefineForClassFields")
            && let Some(boolean) = get_bool(value)
        {
            result.settings.use_define_for_class_fields = maybe_bool(boolean);
        }

        if let Some((value, key_loc)) = get_property(compiler_options, "target")
            && let Some(text) = get_string(value)
        {
            let lower_value = text.to_lowercase();
            let target = match lower_value.as_str() {
                "es3" | "es5" | "es6" | "es2015" | "es2016" | "es2017" | "es2018" | "es2019"
                | "es2020" | "es2021" => Some(TsTarget::BelowEs2022),
                "es2022" | "es2023" | "es2024" | "es2025" | "esnext" => {
                    Some(TsTarget::AtOrAboveEs2022)
                }
                _ => None,
            };
            if let Some(target) = target {
                result.settings.target = target;
                result.ts_target_key = TsTargetKey {
                    source: source.clone(),
                    range: source.range_of_string(key_loc),
                    lower_value,
                };
            } else if !is_inside_node_modules(&source.key_path.text) {
                log.add_id(
                    MsgId::TsConfigJsonInvalidTarget,
                    MsgKind::Warning,
                    Some(&mut tracker),
                    source.range_of_string(value.loc),
                    format!("Unrecognized target environment {text:?}"),
                );
            }
        }

        if let Some((value, key_loc)) = get_property(compiler_options, "strict")
            && let Some(boolean) = get_bool(value)
        {
            let value_range = range_of_identifier(source, value.loc);
            result.ts_strict = Some(TsAlwaysStrict {
                name: "strict".into(),
                value: boolean,
                source: source.clone(),
                range: Range {
                    loc: key_loc,
                    len: value_range.end() - key_loc.start,
                },
            });
        }
        if let Some((value, key_loc)) = get_property(compiler_options, "alwaysStrict")
            && let Some(boolean) = get_bool(value)
        {
            let value_range = range_of_identifier(source, value.loc);
            result.ts_always_strict = Some(TsAlwaysStrict {
                name: "alwaysStrict".into(),
                value: boolean,
                source: source.clone(),
                range: Range {
                    loc: key_loc,
                    len: value_range.end() - key_loc.start,
                },
            });
        }

        if let Some((value, _)) = get_property(compiler_options, "importsNotUsedAsValues")
            && let Some(text) = get_string(value)
        {
            match text.as_str() {
                "remove" => {
                    result.settings.imports_not_used_as_values = TsImportsNotUsedAsValues::Remove;
                }
                "preserve" => {
                    result.settings.imports_not_used_as_values = TsImportsNotUsedAsValues::Preserve;
                }
                "error" => {
                    result.settings.imports_not_used_as_values = TsImportsNotUsedAsValues::Error;
                }
                _ => {
                    log.add_id(
                        MsgId::TsConfigJsonInvalidImportsNotUsedAsValues,
                        MsgKind::Warning,
                        Some(&mut tracker),
                        source.range_of_string(value.loc),
                        format!("Invalid value {text:?} for \"importsNotUsedAsValues\""),
                    );
                }
            }
        }
        if let Some((value, _)) = get_property(compiler_options, "preserveValueImports")
            && let Some(boolean) = get_bool(value)
        {
            result.settings.preserve_value_imports = maybe_bool(boolean);
        }
        if let Some((value, _)) = get_property(compiler_options, "verbatimModuleSyntax")
            && let Some(boolean) = get_bool(value)
        {
            result.settings.verbatim_module_syntax = maybe_bool(boolean);
        }

        if let Some((value, _)) = get_property(compiler_options, "paths")
            && let Some(ExprData::Object(paths)) = value.data.as_deref()
        {
            let mut parsed_paths = TsConfigPaths {
                source: source.clone(),
                ..TsConfigPaths::default()
            };
            result.base_url_for_paths = file_dir.to_string();
            for property in &paths.properties {
                let Some(key) = get_string(&property.key) else {
                    continue;
                };
                if !is_valid_tsconfig_path_pattern(
                    &key,
                    log,
                    source,
                    &mut tracker,
                    property.key.loc,
                ) {
                    continue;
                }
                if let Some(ExprData::Array(array)) = property.value_or_nil.data.as_deref() {
                    for item in &array.items {
                        if let Some(mut text) = get_string(item)
                            && is_valid_tsconfig_path_pattern(
                                &text,
                                log,
                                source,
                                &mut tracker,
                                item.loc,
                            )
                        {
                            text = substituted_path_with_config_dir_template(
                                file_system,
                                &text,
                                config_dir,
                            );
                            parsed_paths
                                .map
                                .entry(key.clone())
                                .or_default()
                                .push(TsConfigPath {
                                    text,
                                    loc: item.loc,
                                });
                        }
                    }
                } else {
                    log.add_id(
                        MsgId::TsConfigJsonInvalidPaths,
                        MsgKind::Warning,
                        Some(&mut tracker),
                        source.range_of_string(property.value_or_nil.loc),
                        format!("Substitutions for pattern {key:?} should be an array"),
                    );
                }
            }
            result.paths = Some(parsed_paths);
        }
    }

    if let Some(ExprData::Object(object)) = json.data.as_deref() {
        'properties: for property in &object.properties {
            let Some(key) = get_string(&property.key) else {
                continue;
            };
            if matches!(
                key.as_str(),
                "alwaysStrict"
                    | "baseUrl"
                    | "experimentalDecorators"
                    | "importsNotUsedAsValues"
                    | "jsx"
                    | "jsxFactory"
                    | "jsxFragmentFactory"
                    | "jsxImportSource"
                    | "paths"
                    | "preserveValueImports"
                    | "strict"
                    | "target"
                    | "useDefineForClassFields"
                    | "verbatimModuleSyntax"
            ) {
                log.add_id_with_notes(
                    MsgId::TsConfigJsonInvalidTopLevelOption,
                    MsgKind::Warning,
                    Some(&mut tracker),
                    source.range_of_string(property.key.loc),
                    format!(
                        "Expected the {key:?} option to be nested inside a \"compilerOptions\" object"
                    ),
                    Vec::new(),
                );
                break 'properties;
            }
        }
    }

    Some(result)
}

#[must_use]
pub fn substituted_path_with_config_dir_template(
    file_system: &dyn Fs,
    value: &str,
    base_path: &str,
) -> String {
    value.strip_prefix("${configDir}").map_or_else(
        || value.to_string(),
        |suffix| file_system.join(&[base_path, &format!("./{suffix}")]),
    )
}

fn parse_member_expression_for_jsx(
    log: &Log,
    source: &Source,
    tracker: &mut LineColumnTracker,
    location: Loc,
    text: &str,
) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let parts: Vec<String> = text.split('.').map(str::to_string).collect();
    if parts
        .iter()
        .any(|part| !crate::internal::js_ast::is_identifier(part))
    {
        log.add_id(
            MsgId::TsConfigJsonInvalidJsx,
            MsgKind::Warning,
            Some(tracker),
            source.range_of_string(location),
            format!("Invalid JSX member expression: {text:?}"),
        );
        return Vec::new();
    }
    parts
}

fn is_valid_tsconfig_path_pattern(
    text: &str,
    log: &Log,
    source: &Source,
    tracker: &mut LineColumnTracker,
    location: Loc,
) -> bool {
    if text.bytes().filter(|&byte| byte == b'*').count() <= 1 {
        return true;
    }
    log.add_id(
        MsgId::TsConfigJsonInvalidPaths,
        MsgKind::Warning,
        Some(tracker),
        source.range_of_string(location),
        format!("Invalid pattern {text:?}, must have at most one \"*\" character"),
    );
    false
}

#[allow(dead_code)]
fn is_slash(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\')
}

#[allow(dead_code)]
fn is_valid_tsconfig_path_no_base_url_pattern(
    text: &str,
    log: &Log,
    source: &Source,
    tracker: &mut Option<LineColumnTracker>,
    location: Loc,
) -> bool {
    let bytes = text.as_bytes();
    let c0 = bytes.first().copied().unwrap_or_default();
    let c1 = bytes.get(1).copied().unwrap_or_default();
    let c2 = bytes.get(2).copied().unwrap_or_default();
    let length = bytes.len();
    if (c0 == b'.' && (length == 1 || (length == 2 && c1 == b'.')))
        || (c0 == b'.' && (is_slash(c1) || (c1 == b'.' && is_slash(c2))))
        || is_slash(c0)
        || (c0.is_ascii_alphabetic() && c1 == b':' && is_slash(c2))
    {
        return true;
    }
    let tracker = tracker.get_or_insert_with(|| LineColumnTracker::new(Some(source)));
    log.add_id(
        MsgId::TsConfigJsonInvalidPaths,
        MsgKind::Warning,
        Some(tracker),
        source.range_of_string(location),
        format!(
            "Non-relative path {text:?} is not allowed when \"baseUrl\" is not set (did you forget a leading \"./\"?)"
        ),
    );
    false
}

fn get_property<'a>(expression: &'a Expr, name: &str) -> Option<(&'a Expr, Loc)> {
    let ExprData::Object(object) = expression.data.as_deref()? else {
        return None;
    };
    object.properties.iter().find_map(|property| {
        (get_string(&property.key).as_deref() == Some(name))
            .then_some((&property.value_or_nil, property.key.loc))
    })
}

fn get_string(expression: &Expr) -> Option<String> {
    let ExprData::String(string) = expression.data.as_deref()? else {
        return None;
    };
    Some(String::from_utf8_lossy(&utf16_to_string(&string.value)).into_owned())
}

fn get_bool(expression: &Expr) -> Option<bool> {
    let ExprData::Boolean(value) = expression.data.as_deref()? else {
        return None;
    };
    Some(*value)
}

const fn maybe_bool(value: bool) -> MaybeBool {
    if value {
        MaybeBool::True
    } else {
        MaybeBool::False
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataUrl {
    mime_type: String,
    data: String,
    is_base64: bool,
}

impl DataUrl {
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let contents = url.strip_prefix("data:")?;
        let comma = contents.find(',')?;
        let (mut mime_type, data_with_comma) = contents.split_at(comma);
        let mut is_base64 = false;
        if let Some(without_base64) = mime_type.strip_suffix(";base64") {
            mime_type = without_base64;
            is_base64 = true;
        }
        Some(Self {
            mime_type: mime_type.to_string(),
            data: data_with_comma[1..].to_string(),
            is_base64,
        })
    }

    #[must_use]
    pub fn decode_mime_type(&self) -> MimeType {
        match self
            .mime_type
            .split_once(';')
            .map_or(self.mime_type.as_str(), |(mime_type, _)| mime_type)
        {
            "text/css" => MimeType::TextCss,
            "text/javascript" => MimeType::TextJavaScript,
            "application/json" => MimeType::ApplicationJson,
            _ => MimeType::Unsupported,
        }
    }

    /// # Errors
    ///
    /// Returns an error when base64 or percent-escaped data is malformed.
    pub fn decode_data(&self) -> Result<Vec<u8>, String> {
        if self.is_base64 {
            return STANDARD
                .decode(self.data.as_bytes())
                .map_err(|error| format!("could not decode base64 data: {error}"));
        }
        decode_percent_escaped(self.data.as_bytes())
            .map_err(|error| format!("could not decode percent-escaped data: {error}"))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MimeType {
    #[default]
    Unsupported,
    TextCss,
    TextJavaScript,
    ApplicationJson,
}

fn decode_percent_escaped(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut decoded = Vec::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
        if data[index] != b'%' {
            decoded.push(data[index]);
            index += 1;
            continue;
        }
        if index + 2 >= data.len() {
            return Err("invalid URL escape");
        }
        let Some(high) = hex_value(data[index + 1]) else {
            return Err("invalid URL escape");
        };
        let Some(low) = hex_value(data[index + 2]) else {
            return Err("invalid URL escape");
        };
        decoded.push((high << 4) | low);
        index += 3;
    }
    Ok(decoded)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{
        DataUrl, DebugMeta, MimeType, PackageMapKind, PathPair, TsConfigJson, TsConfigPath,
        TsConfigPaths, find_invalid_package_segment, globstar_to_escaped_regexp,
        is_valid_tsconfig_path_no_base_url_pattern, match_tsconfig_path_candidates,
        parse_bare_identifier, parse_esm_package_name, parse_imports_exports_map,
        parse_tsconfig_json, sort_package_expansion_keys,
    };
    use crate::internal::{
        config::{MaybeBool, TsJsx, TsTarget},
        fs::{MockKind, mock_fs},
        js_parser::{JsonOptions, parse_json},
        logger::{DeferLogKind, Loc, Log, Path, PrettyPaths, Range, Source},
    };

    #[test]
    fn path_pair_iterates_primary_and_optional_secondary() {
        let mut pair = PathPair {
            primary: Path {
                text: "module.js".into(),
                ..Path::default()
            },
            ..PathPair::default()
        };
        assert_eq!(pair.iter_mut().count(), 1);
        pair.secondary.text = "main.js".into();
        assert_eq!(
            pair.iter_mut()
                .map(|path| path.text.clone())
                .collect::<Vec<_>>(),
            vec!["module.js", "main.js"]
        );
    }

    #[test]
    fn parses_and_decodes_data_urls_like_upstream() {
        let css = DataUrl::parse("data:text/css;charset=utf-8,a+b%20c").expect("data URL");
        assert_eq!(css.decode_mime_type(), MimeType::TextCss);
        assert_eq!(css.decode_data().expect("percent data"), b"a+b c");

        let json =
            DataUrl::parse("data:application/json;base64,eyJhIjoxfQ==").expect("base64 data URL");
        assert_eq!(json.decode_mime_type(), MimeType::ApplicationJson);
        assert_eq!(json.decode_data().expect("base64 data"), br#"{"a":1}"#);

        assert!(DataUrl::parse("text/css,body{}").is_none());
        assert!(DataUrl::parse("data:text/css").is_none());
    }

    #[test]
    fn data_url_decoding_preserves_arbitrary_bytes_and_rejects_bad_escapes() {
        let binary = DataUrl::parse("data:text/javascript,%FF").expect("data URL");
        assert_eq!(binary.decode_data().expect("arbitrary bytes"), vec![0xff]);
        assert!(
            DataUrl::parse("data:text/css,%xy")
                .expect("data URL")
                .decode_data()
                .expect_err("invalid escape")
                .starts_with("could not decode percent-escaped data:")
        );
        assert!(
            DataUrl::parse("data:text/css;base64,?")
                .expect("data URL")
                .decode_data()
                .expect_err("invalid base64")
                .starts_with("could not decode base64 data:")
        );
    }

    #[test]
    fn debug_meta_attaches_source_suggestions_and_notes() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            pretty_paths: PrettyPaths {
                abs: "/entry.js".into(),
                rel: "entry.js".into(),
            },
            contents: Arc::from(&b"import 'bad'"[..]),
            ..Source::default()
        };
        DebugMeta::default().log_error_msg(
            &log,
            Some(&source),
            Range {
                loc: Loc { start: 8 },
                len: 5,
            },
            "Could not resolve",
            "'good'",
            &[],
        );
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data.text, "Could not resolve");
        assert_eq!(
            messages[0]
                .data
                .location
                .as_ref()
                .expect("location")
                .suggestion,
            "'good'"
        );
    }

    #[test]
    fn parses_tsconfig_settings_paths_and_extends_in_order() {
        let contents = r#"{
          "extends": ["base", "second"],
          "compilerOptions": {
            "baseUrl": "${configDir}/src",
            "jsx": "react-jsx",
            "jsxFactory": "React.createElement",
            "experimentalDecorators": true,
            "useDefineForClassFields": false,
            "target": "ES2022",
            "strict": true,
            "importsNotUsedAsValues": "preserve",
            "preserveValueImports": true,
            "verbatimModuleSyntax": true,
            "paths": {
              "@/*": ["${configDir}/lib/*"],
              "invalid**": ["ignored"]
            }
          }
        }"#;
        let source = Source {
            key_path: Path {
                text: "/project/tsconfig.json".into(),
                ..Path::default()
            },
            pretty_paths: PrettyPaths {
                abs: "/project/tsconfig.json".into(),
                rel: "tsconfig.json".into(),
            },
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        };
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut seen = Vec::new();
        let mut extends = |path: &str, _range: Range| {
            seen.push(path.to_string());
            Some(TsConfigJson {
                settings: crate::internal::config::TsConfig {
                    experimental_decorators: MaybeBool::False,
                    ..crate::internal::config::TsConfig::default()
                },
                ..TsConfigJson::default()
            })
        };

        let config = parse_tsconfig_json(
            &log,
            &source,
            &file_system,
            "/project",
            "/configs",
            Some(&mut extends),
        )
        .expect("valid tsconfig");

        assert_eq!(seen, vec!["base", "second"]);
        assert_eq!(config.base_url.as_deref(), Some("/configs/src"));
        assert_eq!(config.jsx_settings.jsx, TsJsx::ReactJsx);
        assert_eq!(
            config.jsx_settings.jsx_factory,
            vec!["React", "createElement"]
        );
        assert_eq!(config.settings.experimental_decorators, MaybeBool::True);
        assert_eq!(
            config.settings.use_define_for_class_fields,
            MaybeBool::False
        );
        assert_eq!(config.settings.target, TsTarget::AtOrAboveEs2022);
        assert!(config.ts_always_strict_or_strict().is_some_and(|x| x.value));
        assert_eq!(
            config
                .paths
                .as_ref()
                .expect("paths")
                .map
                .get("@/*")
                .expect("mapping")[0]
                .text,
            "/configs/lib/*"
        );
        assert!(
            !config
                .paths
                .as_ref()
                .expect("paths")
                .map
                .contains_key("invalid**")
        );
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn validates_paths_without_base_url_like_typescript() {
        let source = Source {
            contents: Arc::from(&b"\"package\""[..]),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut tracker = None;
        assert!(is_valid_tsconfig_path_no_base_url_pattern(
            "../generated/*",
            &log,
            &source,
            &mut tracker,
            Loc::default()
        ));
        assert!(!is_valid_tsconfig_path_no_base_url_pattern(
            "package/*",
            &log,
            &source,
            &mut tracker,
            Loc::default()
        ));
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn matches_tsconfig_paths_with_typescript_precedence() {
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let config = TsConfigJson {
            base_url: Some("/explicit".into()),
            base_url_for_paths: "/implicit".into(),
            paths: Some(TsConfigPaths {
                map: HashMap::from([
                    (
                        "exact".into(),
                        vec![
                            TsConfigPath {
                                text: "types.d.ts".into(),
                                ..TsConfigPath::default()
                            },
                            TsConfigPath {
                                text: "exact.js".into(),
                                ..TsConfigPath::default()
                            },
                        ],
                    ),
                    (
                        "a*".into(),
                        vec![TsConfigPath {
                            text: "short/*".into(),
                            ..TsConfigPath::default()
                        }],
                    ),
                    (
                        "abc*".into(),
                        vec![TsConfigPath {
                            text: "long/*".into(),
                            ..TsConfigPath::default()
                        }],
                    ),
                    (
                        "abc*xyz".into(),
                        vec![TsConfigPath {
                            text: "/absolute/*".into(),
                            ..TsConfigPath::default()
                        }],
                    ),
                ]),
                ..TsConfigPaths::default()
            }),
            ..TsConfigJson::default()
        };

        assert_eq!(
            match_tsconfig_path_candidates(&config, "exact", &file_system)
                .expect("exact match")
                .iter()
                .map(|candidate| candidate.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/explicit/exact.js"]
        );
        assert_eq!(
            match_tsconfig_path_candidates(&config, "abcdef", &file_system)
                .expect("longest prefix")
                .iter()
                .map(|candidate| candidate.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/explicit/long/def"]
        );
        assert_eq!(
            match_tsconfig_path_candidates(&config, "abcHELLOxyz", &file_system)
                .expect("longest suffix tie-break")
                .iter()
                .map(|candidate| candidate.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/absolute/HELLO"]
        );
        assert!(match_tsconfig_path_candidates(&config, "missing", &file_system).is_none());
    }

    #[test]
    fn parses_yarn_and_node_package_specifiers() {
        assert_eq!(
            parse_bare_identifier("@scope/pkg/subpath"),
            Some(("@scope/pkg", "/subpath"))
        );
        assert_eq!(parse_bare_identifier("@scope"), None);
        assert_eq!(
            parse_bare_identifier("package/subpath"),
            Some(("package", "/subpath"))
        );
        assert_eq!(
            parse_esm_package_name("@scope/pkg/subpath"),
            Some(("@scope/pkg", "./subpath".into()))
        );
        assert_eq!(
            parse_esm_package_name("package"),
            Some(("package", ".".into()))
        );
        assert_eq!(parse_esm_package_name("../package"), None);
        assert_eq!(parse_esm_package_name("bad%name"), None);
    }

    #[test]
    fn validates_package_segments_and_converts_globstars() {
        assert_eq!(
            find_invalid_package_segment("./ok/node_modules/pkg"),
            Some("node_modules")
        );
        assert_eq!(find_invalid_package_segment("./ok/../pkg"), Some(".."));
        assert_eq!(find_invalid_package_segment("node_modules/pkg"), None);

        let (pattern, wildcard) = globstar_to_escaped_regexp("src/**/test?.[jt]s");
        assert!(wildcard);
        let regexp = regex::Regex::new(&pattern).expect("generated regular expression");
        assert!(regexp.is_match("src/unit/test1.[jt]s"));
        assert!(regexp.is_match("src/testx.[jt]s"));
        assert!(!regexp.is_match("src/unit/deep/testing.[jt]s"));
        assert_eq!(
            globstar_to_escaped_regexp("file.js"),
            ("^file\\.js$".into(), false)
        );
    }

    #[test]
    fn sorts_package_expansion_keys_deterministically() {
        assert_eq!(
            sort_package_expansion_keys(["./foo/", "./foo*", "./foo*bar", "./*"]),
            vec!["./foo/", "./foo*bar", "./foo*", "./*"]
        );
    }

    #[test]
    fn parses_ordered_package_maps_and_sorts_expansions() {
        let contents = r#"{"./foo*":"a","./foo*bar":"b","./foo/":"c"}"#;
        let source = Source {
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
        assert!(ok);
        let map = parse_imports_exports_map(&source, &log, &json, "exports", Loc::default())
            .expect("package map");
        assert_eq!(map.root.kind, PackageMapKind::Object);
        assert!(map.root.keys_start_with_dot());
        assert_eq!(
            map.root
                .expansion_keys
                .iter()
                .map(|property| property.key.as_str())
                .collect::<Vec<_>>(),
            vec!["./foo/", "./foo*bar", "./foo*"]
        );
        assert_eq!(
            map.root
                .value_for_key("./foo*bar")
                .expect("map value")
                .string,
            "b"
        );
        assert!(log.done().is_empty());
    }

    #[test]
    fn package_maps_reject_mixed_keys_and_warn_for_dead_conditions() {
        for (contents, expected_kind) in [
            (
                r#"{"./subpath":"./file.js","default":"./other.js"}"#,
                PackageMapKind::Invalid,
            ),
            (
                r#"{"default":"./file.js","browser":"./other.js"}"#,
                PackageMapKind::Object,
            ),
            ("true", PackageMapKind::Invalid),
        ] {
            let source = Source {
                contents: Arc::from(contents.as_bytes()),
                ..Source::default()
            };
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
            assert!(ok);
            let map = parse_imports_exports_map(&source, &log, &json, "exports", Loc::default())
                .expect("non-null map");
            assert_eq!(map.root.kind, expected_kind);
            assert_eq!(log.done().len(), 1);
        }

        let source = Source {
            contents: Arc::from(&b"null"[..]),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
        assert!(ok);
        assert!(
            parse_imports_exports_map(&source, &log, &json, "exports", Loc::default()).is_none()
        );
    }
}
