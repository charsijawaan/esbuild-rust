//! Port of upstream `internal/bundler`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::internal::{
    ast::Index32,
    compat::{CssFeature, JsFeature},
    config::{Loader, Options, PathPlaceholder, PathTemplate, Platform, PluginData},
    css_parser,
    fs::Fs,
    graph::{
        CssRepr, EntryPoint as GraphEntryPoint, InputFile, InputFileRepr, JsRepr, SideEffects,
        SideEffectsKind,
    },
    helpers::{encode_string_as_shortest_data_url, mime_type_by_extension, string_to_utf16},
    js_ast::{self, ExportsKind, Expr, ExprData, StringExpr},
    js_parser::{self, HelperCall},
    logger::{self, Log, Range, Source},
    runtime,
    sourcemap::LineOffsetTable,
};

#[derive(Clone, Default)]
pub struct ScannerFile {
    pub json_metadata_chunk: String,
    pub plugin_data: Option<PluginData>,
    pub input_file: InputFile,
}

#[derive(Clone, Default)]
pub struct ParseResult {
    pub file: ScannerFile,
    pub tla_check: TlaCheck,
    pub ok: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TlaCheck {
    pub parent: Index32,
    pub depth: u32,
    pub import_record_index: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DataForSourceMap {
    pub line_offset_tables: Vec<LineOffsetTable>,
    pub quoted_contents: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EntryPoint {
    pub input_path: String,
    pub output_path: String,
    pub input_path_in_file_namespace: bool,
}

#[must_use]
pub fn default_extension_to_loader_map() -> HashMap<String, Loader> {
    [
        ("", Loader::Js),
        (".js", Loader::Js),
        (".mjs", Loader::Js),
        (".cjs", Loader::Js),
        (".jsx", Loader::Jsx),
        (".ts", Loader::Ts),
        (".cts", Loader::TsNoAmbiguousLessThan),
        (".mts", Loader::TsNoAmbiguousLessThan),
        (".tsx", Loader::Tsx),
        (".css", Loader::Css),
        (".module.css", Loader::LocalCss),
        (".json", Loader::Json),
        (".txt", Loader::Text),
    ]
    .into_iter()
    .map(|(extension, loader)| (extension.to_string(), loader))
    .collect()
}

#[must_use]
pub fn parse_file(log: &Log, source: Source, loader: Loader, options: &Options) -> ParseResult {
    parse_file_with_unique_key_prefix(log, source, loader, options, "")
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn parse_file_with_unique_key_prefix(
    log: &Log,
    mut source: Source,
    mut loader: Loader,
    options: &Options,
    unique_key_prefix: &str,
) -> ParseResult {
    let (_, base, extension) =
        logger::platform_independent_path_dir_base_ext(&source.key_path.text);
    if loader == Loader::Default {
        loader = crate::internal::config::loader_from_file_extension(
            &options.extension_to_loader,
            &format!("{base}{extension}"),
        );
    }
    if source.identifier_name.is_empty() {
        source.identifier_name = js_ast::generate_non_unique_name_from_path(&source.key_path.text);
    }
    if loader == Loader::Empty {
        source.contents = Arc::from(&b""[..]);
    }

    let mut result = ParseResult {
        file: ScannerFile {
            input_file: InputFile {
                source: source.clone(),
                loader,
                side_effects: SideEffects::default(),
                ..InputFile::default()
            },
            ..ScannerFile::default()
        },
        ..ParseResult::default()
    };

    match loader {
        Loader::Js
        | Loader::Jsx
        | Loader::Ts
        | Loader::TsNoAmbiguousLessThan
        | Loader::Tsx
        | Loader::Empty => {
            let mut parser_options = js_parser::options_from_config(options);
            parser_options.jsx.parse = matches!(loader, Loader::Jsx | Loader::Tsx);
            parser_options.ts.parse = matches!(
                loader,
                Loader::Ts | Loader::TsNoAmbiguousLessThan | Loader::Tsx
            );
            parser_options.ts.no_ambiguous_less_than = loader == Loader::TsNoAmbiguousLessThan;
            let (ast, ok) = js_parser::parse(log.clone(), source, parser_options);
            if ast.parts.len() <= 1 {
                result.file.input_file.side_effects.kind = SideEffectsKind::NoSideEffectsEmptyAst;
            }
            result.file.input_file.repr = Some(InputFileRepr::Js(Box::new(JsRepr {
                ast,
                ..JsRepr::default()
            })));
            result.ok = ok;
        }
        Loader::Css | Loader::GlobalCss | Loader::LocalCss => {
            let ast = css_parser::parse(
                log.clone(),
                source,
                css_parser::Options {
                    minify_syntax: options.minify_syntax,
                    minify_whitespace: options.minify_whitespace,
                    minify_identifiers: options.minify_identifiers,
                    symbol_mode: match loader {
                        Loader::LocalCss => css_parser::SymbolMode::Local,
                        Loader::GlobalCss => css_parser::SymbolMode::Global,
                        _ => css_parser::SymbolMode::Disabled,
                    },
                },
            );
            result.file.input_file.repr = Some(InputFileRepr::Css(Box::new(CssRepr {
                ast,
                ..CssRepr::default()
            })));
            result.ok = true;
        }
        Loader::Json | Loader::WithTypeJson => {
            let (expression, ok) = js_parser::parse_json(
                log.clone(),
                source.clone(),
                js_parser::JsonOptions {
                    unsupported_js_features: options.unsupported_js_features,
                    ..js_parser::JsonOptions::default()
                },
            );
            let mut ast = js_parser::lazy_export_ast(
                log.clone(),
                &source,
                js_parser::options_from_config(options),
                expression,
                None,
            );
            if loader == Loader::WithTypeJson {
                ast.exports_kind = ExportsKind::Esm;
            }
            result.file.input_file.side_effects.kind = SideEffectsKind::NoSideEffectsPureData;
            result.file.input_file.repr = Some(InputFileRepr::Js(Box::new(JsRepr {
                ast,
                ..JsRepr::default()
            })));
            result.ok = ok;
        }
        Loader::Text => {
            let contents = source
                .contents
                .strip_prefix(&[0xef, 0xbb, 0xbf])
                .unwrap_or(&source.contents)
                .to_vec();
            source.contents = Arc::from(contents.clone());
            result.file.input_file.source.contents = source.contents.clone();
            let encoded = STANDARD.encode(&contents);
            let mut ast = lazy_export_string(log, &source, options, &contents, None);
            ast.url_for_css = format!("data:text/plain;base64,{encoded}");
            set_pure_data_result(&mut result, ast);
        }
        Loader::Base64 => {
            let encoded = STANDARD.encode(&source.contents);
            let mime_type = guess_mime_type(&extension, &source.contents);
            let mut ast = lazy_export_string(log, &source, options, encoded.as_bytes(), None);
            ast.url_for_css = format!("data:{mime_type};base64,{encoded}");
            set_pure_data_result(&mut result, ast);
        }
        Loader::Binary => {
            let encoded = STANDARD.encode(&source.contents);
            let helper_call = if options
                .unsupported_js_features
                .contains(JsFeature::FROM_BASE64)
            {
                HelperCall {
                    runtime: if options.platform == Platform::Node {
                        "__toBinaryNode".into()
                    } else {
                        "__toBinary".into()
                    },
                    ..HelperCall::default()
                }
            } else {
                HelperCall {
                    global: vec!["Uint8Array".into(), "fromBase64".into()],
                    ..HelperCall::default()
                }
            };
            let mut ast = lazy_export_string(
                log,
                &source,
                options,
                encoded.as_bytes(),
                Some(&helper_call),
            );
            ast.url_for_css = format!("data:application/octet-stream;base64,{encoded}");
            set_pure_data_result(&mut result, ast);
        }
        Loader::DataUrl => {
            let mime_type = guess_mime_type(&extension, &source.contents);
            let mut url = encode_string_as_shortest_data_url(&mime_type, &source.contents);
            if source.key_path.ignored_suffix.starts_with('#') {
                url.push_str(&source.key_path.ignored_suffix);
            }
            let mut ast = lazy_export_string(log, &source, options, url.as_bytes(), None);
            ast.url_for_css.clone_from(&url);
            set_pure_data_result(&mut result, ast);
        }
        Loader::File => {
            let unique_key = format!("{unique_key_prefix}A{:08}", source.index);
            let unique_key_path = format!("{unique_key}{}", source.key_path.ignored_suffix);
            let expression = Expr::new(
                logger::Loc::default(),
                ExprData::String(StringExpr {
                    value: string_to_utf16(unique_key_path.as_bytes()),
                    contains_unique_key: true,
                    ..StringExpr::default()
                }),
            );
            let mut ast = js_parser::lazy_export_ast(
                log.clone(),
                &source,
                js_parser::options_from_config(options),
                expression,
                None,
            );
            ast.url_for_css = unique_key_path;
            result.file.input_file.unique_key_for_additional_file = unique_key;
            set_pure_data_result(&mut result, ast);
        }
        Loader::Copy => {
            let unique_key = format!("{unique_key_prefix}A{:08}", source.index);
            let unique_key_path = format!("{unique_key}{}", source.key_path.ignored_suffix);
            result.file.input_file.unique_key_for_additional_file = unique_key;
            result.file.input_file.repr =
                Some(InputFileRepr::Copy(crate::internal::graph::CopyRepr {
                    url_for_code: unique_key_path,
                }));
            result.ok = true;
        }
        _ => {
            let display_path = if source.pretty_paths.rel.is_empty() {
                &source.key_path.text
            } else {
                source.pretty_paths.select(options.log_path_style)
            };
            let message = if source.key_path.namespace == "file" && !extension.is_empty() {
                format!("No loader is configured for {extension:?} files: {display_path}")
            } else {
                format!("Do not know how to load path: {display_path}")
            };
            log.add_error(None, Range::default(), message);
        }
    }

    result
}

fn lazy_export_string(
    log: &Log,
    source: &Source,
    options: &Options,
    value: &[u8],
    helper_call: Option<&HelperCall>,
) -> js_ast::Ast {
    js_parser::lazy_export_ast(
        log.clone(),
        source,
        js_parser::options_from_config(options),
        Expr::new(
            logger::Loc::default(),
            ExprData::String(StringExpr {
                value: string_to_utf16(value),
                ..StringExpr::default()
            }),
        ),
        helper_call,
    )
}

fn set_pure_data_result(result: &mut ParseResult, ast: js_ast::Ast) {
    result.file.input_file.side_effects.kind = SideEffectsKind::NoSideEffectsPureData;
    result.file.input_file.repr = Some(InputFileRepr::Js(Box::new(JsRepr {
        ast,
        ..JsRepr::default()
    })));
    result.ok = true;
}

#[must_use]
pub fn guess_mime_type(extension: &str, contents: &[u8]) -> String {
    let known = mime_type_by_extension(extension);
    let mime_type = if known.is_empty() {
        detect_content_type(contents)
    } else {
        known
    };
    mime_type.replace("; ", ";")
}

fn detect_content_type(contents: &[u8]) -> &'static str {
    if contents.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if contents.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if contents.starts_with(b"GIF87a") || contents.starts_with(b"GIF89a") {
        "image/gif"
    } else if contents.starts_with(b"%PDF-") {
        "application/pdf"
    } else if contents.starts_with(b"\0asm") {
        "application/wasm"
    } else if contents.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if std::str::from_utf8(contents).is_ok()
        && !contents
            .iter()
            .any(|byte| *byte < 0x20 && !matches!(*byte, b'\t' | b'\n' | b'\r' | b'\x0c'))
    {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

pub fn apply_option_defaults(options: &mut Options) {
    if options.extension_to_loader.is_empty() {
        options.extension_to_loader = default_extension_to_loader_map();
    }
    if options.output_extension_js.is_empty() {
        options.output_extension_js = ".js".into();
    }
    if options.output_extension_css.is_empty() {
        options.output_extension_css = ".css".into();
    }
    if options.entry_path_template.is_empty() {
        options.entry_path_template = vec![
            PathTemplate {
                data: "./".into(),
                placeholder: PathPlaceholder::Dir,
            },
            PathTemplate {
                data: "/".into(),
                placeholder: PathPlaceholder::Name,
            },
        ];
    }
    if options.chunk_path_template.is_empty() {
        options.chunk_path_template = vec![
            PathTemplate {
                data: "./".into(),
                placeholder: PathPlaceholder::Name,
            },
            PathTemplate {
                data: "-".into(),
                placeholder: PathPlaceholder::Hash,
            },
        ];
    }
    if options.asset_path_template.is_empty() {
        options.asset_path_template = vec![
            PathTemplate {
                data: "./".into(),
                placeholder: PathPlaceholder::Name,
            },
            PathTemplate {
                data: "-".into(),
                placeholder: PathPlaceholder::Hash,
            },
        ];
    }
    options.profiler_names = !options.minify_identifiers;

    fix_invalid_unsupported_js_feature_overrides(
        options,
        JsFeature::ASYNC_AWAIT,
        JsFeature::ASYNC_GENERATOR | JsFeature::FOR_AWAIT | JsFeature::TOP_LEVEL_AWAIT,
    );
    fix_invalid_unsupported_js_feature_overrides(
        options,
        JsFeature::GENERATOR,
        JsFeature::ASYNC_GENERATOR,
    );
    fix_invalid_unsupported_js_feature_overrides(
        options,
        JsFeature::OBJECT_ACCESSORS,
        JsFeature::CLASS_PRIVATE_ACCESSOR | JsFeature::CLASS_PRIVATE_STATIC_ACCESSOR,
    );
    fix_invalid_unsupported_js_feature_overrides(
        options,
        JsFeature::CLASS_FIELD,
        JsFeature::CLASS_PRIVATE_FIELD,
    );
    fix_invalid_unsupported_js_feature_overrides(
        options,
        JsFeature::CLASS_STATIC_FIELD,
        JsFeature::CLASS_PRIVATE_STATIC_FIELD,
    );
    fix_invalid_unsupported_js_feature_overrides(
        options,
        JsFeature::CLASS,
        JsFeature::CLASS_FIELD
            | JsFeature::CLASS_PRIVATE_ACCESSOR
            | JsFeature::CLASS_PRIVATE_BRAND_CHECK
            | JsFeature::CLASS_PRIVATE_FIELD
            | JsFeature::CLASS_PRIVATE_METHOD
            | JsFeature::CLASS_PRIVATE_STATIC_ACCESSOR
            | JsFeature::CLASS_PRIVATE_STATIC_FIELD
            | JsFeature::CLASS_PRIVATE_STATIC_METHOD
            | JsFeature::CLASS_STATIC_BLOCKS
            | JsFeature::CLASS_STATIC_FIELD,
    );
    if options.platform != Platform::Browser {
        if !options
            .unsupported_js_feature_overrides_mask
            .contains(JsFeature::INLINE_SCRIPT)
        {
            options.unsupported_js_features |= JsFeature::INLINE_SCRIPT;
        }
        if !options
            .unsupported_css_feature_overrides_mask
            .contains(CssFeature::INLINE_STYLE)
        {
            options.unsupported_css_features |= CssFeature::INLINE_STYLE;
        }
    }
}

fn fix_invalid_unsupported_js_feature_overrides(
    options: &mut Options,
    implies: JsFeature,
    implied_features: JsFeature,
) {
    if options.unsupported_js_feature_overrides.contains(implies) {
        options.unsupported_js_features |= implied_features;
        options.unsupported_js_feature_overrides |= implied_features;
        options.unsupported_js_feature_overrides_mask |= implied_features;
    }
}

#[must_use]
pub fn find_reachable_files(files: &[InputFile], entry_points: &[GraphEntryPoint]) -> Vec<u32> {
    fn visit(
        source_index: u32,
        files: &[InputFile],
        visited: &mut HashSet<u32>,
        order: &mut Vec<u32>,
    ) {
        if !visited.insert(source_index) {
            return;
        }
        let file = &files[source_index as usize];
        if let Some(InputFileRepr::Js(repr)) = &file.repr
            && repr.css_source_index.is_valid()
        {
            visit(repr.css_source_index.get_index(), files, visited, order);
        }
        if let Some(repr) = &file.repr
            && let Some(records) = repr.import_records()
        {
            for record in records {
                if record.source_index.is_valid() {
                    visit(record.source_index.get_index(), files, visited, order);
                } else if record.copy_source_index.is_valid() {
                    visit(record.copy_source_index.get_index(), files, visited, order);
                }
            }
        }
        order.push(source_index);
    }

    let mut visited = HashSet::new();
    let mut order = Vec::new();
    visit(runtime::SOURCE_INDEX, files, &mut visited, &mut order);
    for entry_point in entry_points {
        visit(entry_point.source_index, files, &mut visited, &mut order);
    }
    order
}

#[must_use]
pub fn hash_for_file_name(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut output = String::with_capacity(8);
    let mut buffer = 0_u32;
    let mut bit_count = 0_u8;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bit_count += 8;
        while bit_count >= 5 && output.len() < 8 {
            bit_count -= 5;
            output.push(ALPHABET[((buffer >> bit_count) & 31) as usize] as char);
        }
        if output.len() == 8 {
            return output;
        }
    }
    if bit_count > 0 && output.len() < 8 {
        output.push(ALPHABET[((buffer << (5 - bit_count)) & 31) as usize] as char);
    }
    output
}

#[must_use]
pub fn is_ascii_only(text: &str) -> bool {
    text.chars()
        .all(|character| (' '..='~').contains(&character))
}

#[must_use]
pub fn path_relative_to_outbase(
    input_file: &InputFile,
    options: &Options,
    file_system: &dyn Fs,
    avoid_index: bool,
    custom_file_path: &str,
) -> (String, String) {
    let mut relative_directory = "/".to_string();
    let mut absolute_path = input_file.source.key_path.text.clone();

    if !custom_file_path.is_empty() {
        absolute_path = custom_file_path.to_string();
        if !file_system.is_abs(&absolute_path) {
            absolute_path = file_system.join(&[&options.abs_output_base, &absolute_path]);
        }
    } else if input_file.source.key_path.namespace != "file" {
        let (directory, mut base, _) =
            logger::platform_independent_path_dir_base_ext(&absolute_path);
        if avoid_index && base == "index" {
            (_, base, _) = logger::platform_independent_path_dir_base_ext(&directory);
        }
        return (
            relative_directory,
            sanitize_file_path_for_virtual_module_path(&base),
        );
    } else if avoid_index {
        let base = file_system.base(&absolute_path);
        let extension = file_system.ext(&base);
        let without_extension = &base[..base.len() - extension.len()];
        if without_extension == "index" {
            absolute_path = file_system.dir(&absolute_path);
        }
    }

    let mut base_name;
    if let Some(relative_path) = file_system.rel(&options.abs_output_base, &absolute_path) {
        relative_directory = format!("{}/", file_system.dir(&relative_path)).replace('\\', "/");
        base_name = file_system.base(&relative_path);
        let mut dot_dot_count = 0;
        while relative_directory
            .get(dot_dot_count * 3..)
            .is_some_and(|path| path.starts_with("../"))
        {
            dot_dot_count += 1;
        }
        if dot_dot_count > 0 {
            relative_directory = format!(
                "{}{}",
                "_.._/".repeat(dot_dot_count),
                &relative_directory[dot_dot_count * 3..]
            );
        }
        while relative_directory.ends_with('/') {
            relative_directory.pop();
        }
        relative_directory.insert(0, '/');
        if relative_directory.ends_with("/.") {
            relative_directory.pop();
        }
    } else {
        base_name = file_system.base(&absolute_path);
    }
    if custom_file_path.is_empty() {
        let extension = file_system.ext(&base_name);
        base_name.truncate(base_name.len() - extension.len());
    }
    (relative_directory, base_name)
}

#[must_use]
pub fn sanitize_file_path_for_virtual_module_path(path: &str) -> String {
    let mut result = String::new();
    let mut needs_gap = false;
    for character in path.chars() {
        let invalid = character == '\0'
            || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            || character < ' ';
        if invalid {
            if !result.is_empty() {
                needs_gap = true;
            }
            continue;
        }
        if needs_gap {
            result.push('_');
            needs_gap = false;
        }
        result.push(character);
    }
    if result.is_empty() {
        "_".into()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_option_defaults, default_extension_to_loader_map, find_reachable_files,
        guess_mime_type, hash_for_file_name, is_ascii_only, parse_file,
        parse_file_with_unique_key_prefix, path_relative_to_outbase,
        sanitize_file_path_for_virtual_module_path,
    };
    use crate::internal::{
        ast::{ImportRecord, Index32},
        compat::JsFeature,
        config::{Loader, Options, PathPlaceholder, Platform},
        fs::{MockKind, mock_fs},
        graph::{EntryPoint, InputFile, InputFileRepr, JsRepr, SideEffectsKind},
        js_ast::ExportsKind,
        logger::{DeferLogKind, Log, Path, Source},
        runtime,
    };
    use std::{collections::HashMap, sync::Arc};

    #[test]
    fn applies_upstream_option_defaults() {
        let mut options = Options {
            platform: Platform::Node,
            ..Options::default()
        };
        apply_option_defaults(&mut options);
        assert_eq!(options.extension_to_loader.get(".tsx"), Some(&Loader::Tsx));
        assert_eq!(options.output_extension_js, ".js");
        assert_eq!(options.output_extension_css, ".css");
        assert_eq!(
            options.entry_path_template[0].placeholder,
            PathPlaceholder::Dir
        );
        assert!(options.profiler_names);
        assert!(
            options
                .unsupported_js_features
                .contains(crate::internal::compat::JsFeature::INLINE_SCRIPT)
        );
    }

    #[test]
    fn default_loader_map_matches_upstream_extensions() {
        let map = default_extension_to_loader_map();
        assert_eq!(map.len(), 13);
        assert_eq!(map[".module.css"], Loader::LocalCss);
        assert_eq!(map[""], Loader::Js);
    }

    #[test]
    fn reachable_files_are_dependency_postorder_and_include_runtime() {
        let mut files = vec![InputFile::default(); 4];
        files[0].repr = Some(InputFileRepr::Js(Box::default()));
        files[1].repr = Some(InputFileRepr::Js(Box::new(JsRepr {
            ast: crate::internal::js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    ..ImportRecord::default()
                }],
                ..crate::internal::js_ast::Ast::default()
            },
            ..JsRepr::default()
        })));
        files[2].repr = Some(InputFileRepr::Js(Box::new(JsRepr {
            ast: crate::internal::js_ast::Ast {
                import_records: vec![ImportRecord {
                    copy_source_index: Index32::new(3),
                    ..ImportRecord::default()
                }],
                ..crate::internal::js_ast::Ast::default()
            },
            ..JsRepr::default()
        })));
        files[3].repr = Some(InputFileRepr::Copy(
            crate::internal::graph::CopyRepr::default(),
        ));
        assert_eq!(
            find_reachable_files(
                &files,
                &[EntryPoint {
                    source_index: 1,
                    ..EntryPoint::default()
                }]
            ),
            vec![0, 3, 2, 1]
        );
    }

    #[test]
    fn filename_hash_uses_upstream_base32_prefix() {
        assert_eq!(hash_for_file_name(b"hello"), "NBSWY3DP");
        assert_eq!(hash_for_file_name(b""), "");
    }

    #[test]
    fn sanitizes_virtual_paths_and_checks_printable_ascii() {
        assert_eq!(
            sanitize_file_path_for_virtual_module_path("a<:?>b\0c"),
            "a_b_c"
        );
        assert_eq!(sanitize_file_path_for_virtual_module_path("<>"), "_");
        assert!(is_ascii_only("hello ~"));
        assert!(!is_ascii_only("line\nbreak"));
        assert!(!is_ascii_only("λ"));
    }

    #[test]
    fn computes_paths_relative_to_outbase() {
        let file_system = mock_fs(
            &std::collections::HashMap::new(),
            MockKind::Unix,
            "/project",
        );
        let options = Options {
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let input = InputFile {
            source: Source {
                key_path: Path {
                    text: "/project/src/index.js".into(),
                    namespace: "file".into(),
                    ..Path::default()
                },
                ..Source::default()
            },
            ..InputFile::default()
        };
        assert_eq!(
            path_relative_to_outbase(&input, &options, &file_system, false, ""),
            ("/src".into(), "index".into())
        );
        assert_eq!(
            path_relative_to_outbase(&input, &options, &file_system, true, ""),
            ("/".into(), "src".into())
        );

        let virtual_input = InputFile {
            source: Source {
                key_path: Path {
                    text: "namespace/<bad>/index.js".into(),
                    namespace: "plugin".into(),
                    ..Path::default()
                },
                ..Source::default()
            },
            ..InputFile::default()
        };
        assert_eq!(
            path_relative_to_outbase(&virtual_input, &options, &file_system, true, ""),
            ("/".into(), "bad".into())
        );
        assert_eq!(
            path_relative_to_outbase(&input, &options, &file_system, false, "custom/name.js"),
            ("/custom".into(), "name.js".into())
        );
    }

    fn source(path: &str, contents: &[u8]) -> Source {
        Source {
            key_path: Path {
                text: path.into(),
                namespace: "file".into(),
                ..Path::default()
            },
            contents: Arc::from(contents),
            ..Source::default()
        }
    }

    #[test]
    fn parses_script_loaders_into_graph_files() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut options = Options::default();
        apply_option_defaults(&mut options);

        let js = parse_file(
            &log,
            source("/project/entry.js", b"import './dep.js'; export let x = 1"),
            Loader::Default,
            &options,
        );
        assert!(js.ok);
        assert_eq!(js.file.input_file.loader, Loader::Js);
        assert_eq!(js.file.input_file.source.identifier_name, "entry");
        let Some(InputFileRepr::Js(repr)) = js.file.input_file.repr else {
            panic!("expected a JavaScript representation");
        };
        assert_eq!(repr.ast.import_records.len(), 1);

        let ts = parse_file(
            &log,
            source(
                "/project/types.ts",
                b"interface Point { x: number } export const p: Point = { x: 1 }",
            ),
            Loader::Ts,
            &options,
        );
        assert!(ts.ok);
        assert_eq!(ts.file.input_file.loader, Loader::Ts);

        let empty = parse_file(
            &log,
            source("/project/empty.js", b"this is not valid JavaScript"),
            Loader::Empty,
            &options,
        );
        assert!(empty.ok);
        assert!(empty.file.input_file.source.contents.is_empty());
        assert_eq!(
            empty.file.input_file.side_effects.kind,
            SideEffectsKind::NoSideEffectsEmptyAst
        );
        assert!(log.done().is_empty());
    }

    #[test]
    fn parses_css_loaders_with_their_symbol_modes() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let options = Options {
            minify_identifiers: true,
            ..Options::default()
        };
        let local = parse_file(
            &log,
            source("/project/styles.module.css", b".button { color: red }"),
            Loader::LocalCss,
            &options,
        );
        assert!(local.ok);
        let Some(InputFileRepr::Css(repr)) = local.file.input_file.repr else {
            panic!("expected a CSS representation");
        };
        assert_eq!(repr.ast.local_symbols.len(), 1);
        assert!(repr.ast.char_freq.is_some());
        assert!(log.done().is_empty());
    }

    #[test]
    fn reports_a_missing_loader_during_parsing() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut options = Options::default();
        apply_option_defaults(&mut options);
        let result = parse_file(
            &log,
            source("/project/data.bin", b"\0\x01"),
            Loader::Default,
            &options,
        );
        assert!(!result.ok);
        assert!(result.file.input_file.repr.is_none());
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].data.text,
            "No loader is configured for \".bin\" files: /project/data.bin"
        );
    }

    #[test]
    fn parses_json_text_base64_and_data_url_loaders() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let options = Options::default();

        let json = parse_file(
            &log,
            source("/project/data.json", br#"{"answer": 42}"#),
            Loader::WithTypeJson,
            &options,
        );
        assert!(json.ok);
        assert_eq!(
            json.file.input_file.side_effects.kind,
            SideEffectsKind::NoSideEffectsPureData
        );
        let Some(InputFileRepr::Js(json_repr)) = json.file.input_file.repr else {
            panic!("expected a JSON JavaScript representation");
        };
        assert!(json_repr.ast.has_lazy_export);
        assert_eq!(json_repr.ast.exports_kind, ExportsKind::Esm);

        let text = parse_file(
            &log,
            source("/project/message.txt", b"\xef\xbb\xbfhello"),
            Loader::Text,
            &options,
        );
        assert!(text.ok);
        assert_eq!(&*text.file.input_file.source.contents, b"hello");
        let Some(InputFileRepr::Js(text_repr)) = text.file.input_file.repr else {
            panic!("expected a text JavaScript representation");
        };
        assert_eq!(text_repr.ast.url_for_css, "data:text/plain;base64,aGVsbG8=");

        let base64 = parse_file(
            &log,
            source("/project/image.png", b"\x89PNG\r\n\x1a\n"),
            Loader::Base64,
            &options,
        );
        let Some(InputFileRepr::Js(base64_repr)) = base64.file.input_file.repr else {
            panic!("expected a base64 JavaScript representation");
        };
        assert!(
            base64_repr
                .ast
                .url_for_css
                .starts_with("data:image/png;base64,")
        );

        let data_url = parse_file(
            &log,
            Source {
                key_path: Path {
                    text: "/project/note.txt".into(),
                    namespace: "file".into(),
                    ignored_suffix: "#section".into(),
                    ..Path::default()
                },
                contents: Arc::from(&b"hello"[..]),
                ..Source::default()
            },
            Loader::DataUrl,
            &options,
        );
        let Some(InputFileRepr::Js(data_url_repr)) = data_url.file.input_file.repr else {
            panic!("expected a data URL JavaScript representation");
        };
        assert!(data_url_repr.ast.url_for_css.ends_with("#section"));
        assert!(log.done().is_empty());
    }

    #[test]
    fn parses_binary_file_and_copy_loaders() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let options = Options::default();
        let binary = parse_file(
            &log,
            source("/project/data.bin", &[0, 1, 2]),
            Loader::Binary,
            &options,
        );
        assert!(binary.ok);
        let Some(InputFileRepr::Js(binary_repr)) = binary.file.input_file.repr else {
            panic!("expected a binary JavaScript representation");
        };
        assert!(binary_repr.ast.import_records.is_empty());
        assert_eq!(
            binary_repr.ast.url_for_css,
            "data:application/octet-stream;base64,AAEC"
        );

        let legacy_options = Options {
            unsupported_js_features: JsFeature::FROM_BASE64,
            ..Options::default()
        };
        let legacy_binary = parse_file(
            &log,
            source("/project/legacy.bin", &[0, 1, 2]),
            Loader::Binary,
            &legacy_options,
        );
        let Some(InputFileRepr::Js(legacy_repr)) = legacy_binary.file.input_file.repr else {
            panic!("expected a legacy binary JavaScript representation");
        };
        assert_eq!(
            legacy_repr.ast.import_records[0].source_index.get_index(),
            runtime::SOURCE_INDEX
        );
        assert!(
            legacy_repr
                .ast
                .named_imports
                .values()
                .any(|import| import.alias == "__toBinary")
        );

        let mut file_source = source("/project/asset.svg", b"<svg/>");
        file_source.index = 7;
        file_source.key_path.ignored_suffix = "#icon".into();
        let file = parse_file_with_unique_key_prefix(
            &log,
            file_source.clone(),
            Loader::File,
            &options,
            "UNIQUE",
        );
        assert_eq!(
            file.file.input_file.unique_key_for_additional_file,
            "UNIQUEA00000007"
        );
        let Some(InputFileRepr::Js(file_repr)) = file.file.input_file.repr else {
            panic!("expected a file JavaScript representation");
        };
        assert_eq!(file_repr.ast.url_for_css, "UNIQUEA00000007#icon");

        let copy =
            parse_file_with_unique_key_prefix(&log, file_source, Loader::Copy, &options, "UNIQUE");
        let Some(InputFileRepr::Copy(copy_repr)) = copy.file.input_file.repr else {
            panic!("expected a copy representation");
        };
        assert_eq!(copy_repr.url_for_code, "UNIQUEA00000007#icon");
        assert!(log.done().is_empty());
    }

    #[test]
    fn guesses_mime_types_deterministically() {
        assert_eq!(guess_mime_type(".svg", b"<svg/>"), "image/svg+xml");
        assert_eq!(guess_mime_type("", b"\x89PNG\r\n\x1a\n"), "image/png");
        assert_eq!(
            guess_mime_type("", b"plain text"),
            "text/plain;charset=utf-8"
        );
        assert_eq!(
            guess_mime_type("", &[0xff, 0x00]),
            "application/octet-stream"
        );
    }
}
