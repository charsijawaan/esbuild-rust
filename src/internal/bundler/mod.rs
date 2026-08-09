//! Port of upstream `internal/bundler`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::internal::{
    ast::{AssertOrWithKeyword, ImportKind, ImportRecordFlags, Index32},
    cache::{CacheSet, SourceIndexKind},
    compat::{CssFeature, JsFeature},
    config::{
        self, Loader, Mode, Options, PathPlaceholder, PathPlaceholders, PathTemplate, Platform,
        PluginData, has_placeholder, substitute_template, template_to_string,
    },
    css_parser,
    fs::{EntryKind, Fs},
    graph::{
        CssRepr, EntryPoint as GraphEntryPoint, InputFile, InputFileRepr, JsRepr, OutputFile,
        SideEffects, SideEffectsKind,
    },
    helpers::{
        encode_string_as_shortest_data_url, mime_type_by_extension, quote_for_json, quote_single,
        string_to_utf16, utf16_to_string,
    },
    js_ast::{self, ExportsKind, Expr, ExprData, ModuleType, StringExpr},
    js_parser::{self, HelperCall},
    linker,
    logger::{self, LineColumnTracker, Log, Msg, MsgKind, Path, Range, Source},
    resolver::{self, ResolveResult, ResolverContext},
    runtime,
    sourcemap::LineOffsetTable,
    xxhash,
};

#[derive(Clone, Default)]
pub struct ScannerFile {
    pub json_metadata_chunk: String,
    pub plugin_data: Option<PluginData>,
    pub input_file: InputFile,
}

#[derive(Clone, Default)]
pub struct ParseResult {
    pub resolve_results: Vec<Option<ResolveResult>>,
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

#[derive(Clone, Default)]
pub struct ScannedBundle {
    pub files: Vec<ScannerFile>,
    pub entry_points: Vec<GraphEntryPoint>,
}

#[derive(Debug, Default)]
pub struct CompiledBundle {
    pub metafile: String,
    pub output_files: Vec<OutputFile>,
    pub scan_result: linker::ScanImportsAndExportsResult,
}

/// Convert scanner output directly into the prepared linker graph and chunks.
///
/// # Panics
///
/// Panics when scanner output violates linker invariants or the runtime module
/// is missing required exports.
#[must_use]
pub fn prepare_linker_graph(
    bundle: &ScannedBundle,
    options: &Options,
    unique_key_prefix: &str,
    used_local_names: &mut HashSet<String>,
) -> linker::PreparedLinkerGraph {
    let input_files: Vec<_> = bundle
        .files
        .iter()
        .map(|file| file.input_file.clone())
        .collect();
    let reachable_files = find_reachable_files(&input_files, &bundle.entry_points);
    linker::prepare_linker_graph(
        &input_files,
        &reachable_files,
        &bundle.entry_points,
        options,
        unique_key_prefix,
        used_local_names,
    )
}

/// Compile a scanned JavaScript-only bundle into concrete output files.
///
/// # Panics
///
/// Panics when CSS chunks are present or scanner/linker output violates an
/// internal invariant.
#[must_use]
pub fn compile_javascript_bundle(
    file_system: &dyn Fs,
    bundle: &ScannedBundle,
    options: &Options,
    unique_key_prefix: &str,
) -> CompiledBundle {
    compile_javascript_bundle_with_css_names(
        file_system,
        bundle,
        options,
        unique_key_prefix,
        &mut HashSet::new(),
    )
}

fn compile_javascript_bundle_with_css_names(
    file_system: &dyn Fs,
    bundle: &ScannedBundle,
    options: &Options,
    unique_key_prefix: &str,
    used_local_names: &mut HashSet<String>,
) -> CompiledBundle {
    if !options.code_splitting && bundle.entry_points.len() > 1 {
        let mut compiled = CompiledBundle::default();
        let mut group_options = options.clone();
        group_options.needs_metafile = false;
        for entry_point in &bundle.entry_points {
            let group = compile_javascript_bundle_with_css_names(
                file_system,
                &ScannedBundle {
                    files: bundle.files.clone(),
                    entry_points: vec![entry_point.clone()],
                },
                &group_options,
                unique_key_prefix,
                used_local_names,
            );
            compiled.output_files.extend(group.output_files);
            compiled
                .scan_result
                .import_issues
                .extend(group.scan_result.import_issues);
            compiled
                .scan_result
                .ambiguous_re_exports
                .extend(group.scan_result.ambiguous_re_exports);
            compiled
                .scan_result
                .arbitrary_namespace_issues
                .extend(group.scan_result.arbitrary_namespace_issues);
        }
        deduplicate_identical_output_files(&mut compiled.output_files);
        compiled.metafile =
            generate_metadata_json(file_system, bundle, &compiled.output_files, options);
        return compiled;
    }

    let (mut prepared, source_map_line_offset_tables) =
        if options.source_map == config::SourceMap::None {
            (
                prepare_linker_graph(bundle, options, unique_key_prefix, used_local_names),
                Vec::new(),
            )
        } else {
            std::thread::scope(|scope| {
                let source_map_task =
                    scope.spawn(|| compute_source_map_line_offset_tables(&bundle.files));
                let prepared =
                    prepare_linker_graph(bundle, options, unique_key_prefix, used_local_names);
                let tables = source_map_task
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                (prepared, tables)
            })
        };
    for (file, tables) in prepared
        .graph
        .files
        .iter_mut()
        .zip(source_map_line_offset_tables)
    {
        file.input_file.source_map_line_offset_tables = tables.into();
    }
    let runtime_refs = linker::chunk_runtime_refs_from_graph(
        &prepared.graph,
        (prepared.unbound_module_ref != crate::internal::ast::INVALID_REF)
            .then_some(prepared.unbound_module_ref),
        options.profiler_names,
    );
    let chunk_paths: Vec<_> = prepared
        .chunks
        .iter()
        .map(|chunk| linker::ChunkPath {
            unique_key: chunk.unique_key.clone(),
            final_rel_path: chunk.final_rel_path.clone(),
        })
        .collect();
    let assets: Vec<_> = prepared
        .graph
        .files
        .iter()
        .map(|file| {
            let [additional_file] = file.input_file.additional_files.as_slice() else {
                return None;
            };
            Some(linker::AssetPath {
                unique_key: file.input_file.unique_key_for_additional_file.clone(),
                rel_path: file_system
                    .rel(&options.abs_output_dir, &additional_file.abs_path)
                    .unwrap_or_else(|| additional_file.abs_path.clone()),
            })
        })
        .collect();
    let output_paths = linker::OutputPathContext::new(unique_key_prefix, &assets, &chunk_paths);
    let entry_point_refs = linker::EntryPointTailRefs {
        to_common_js_ref: runtime_refs.to_common_js_ref,
        unbound_module_ref: prepared.unbound_module_ref,
    };
    for chunk_index in 0..prepared.chunks.len() {
        if prepared.chunks[chunk_index].is_css {
            linker::generate_css_chunk(
                &prepared.graph,
                &mut prepared.chunks[chunk_index],
                options,
                &output_paths,
                &prepared.local_css_names,
            );
            continue;
        }
        let renamer = linker::rename_symbols_in_chunk(
            &prepared.graph,
            &prepared.chunks[chunk_index],
            options,
        );
        linker::generate_javascript_chunk(
            &prepared.graph,
            &mut prepared.chunks,
            chunk_index,
            options,
            runtime_refs,
            entry_point_refs,
            renamer.as_ref(),
            &output_paths,
        );
    }
    let mut output_files = linker::finalize_generated_javascript_chunks(
        file_system,
        &prepared.graph,
        &mut prepared.chunks,
        &assets,
        options,
    );
    for file in &prepared.graph.files {
        if file.is_user_specified_entry_point()
            && matches!(file.input_file.repr, Some(InputFileRepr::Copy(_)))
        {
            output_files.extend(file.input_file.additional_files.iter().cloned());
        }
    }
    deduplicate_identical_output_files(&mut output_files);
    let metafile = generate_metadata_json(file_system, bundle, &output_files, options);
    CompiledBundle {
        metafile,
        output_files,
        scan_result: prepared.scan_result,
    }
}

fn deduplicate_identical_output_files(output_files: &mut Vec<OutputFile>) {
    let mut outputs_by_path = HashMap::<String, Vec<u8>>::new();
    output_files.retain(|output| {
        if outputs_by_path
            .get(&output.abs_path)
            .is_some_and(|contents| contents == &output.contents)
        {
            return false;
        }
        outputs_by_path.insert(output.abs_path.clone(), output.contents.clone());
        true
    });
}

fn generate_metadata_json(
    file_system: &dyn Fs,
    bundle: &ScannedBundle,
    output_files: &[OutputFile],
    options: &Options,
) -> String {
    if !options.needs_metafile {
        return String::new();
    }
    let fragment = |text: &str| options.metafile_format.maybe_remove_whitespace(text);
    let mut result = fragment("{\n  \"inputs\": {");
    let mut is_first = true;
    let mut input_paths = HashSet::new();
    for file in &bundle.files {
        if file.input_file.omit_from_source_maps_and_metafile || file.json_metadata_chunk.is_empty()
        {
            continue;
        }
        let path = file
            .input_file
            .source
            .pretty_paths
            .select(options.metafile_path_style);
        if !input_paths.insert(path.to_string()) {
            continue;
        }
        result.push_str(&fragment(if is_first { "\n    " } else { ",\n    " }));
        is_first = false;
        result.push_str(&file.json_metadata_chunk);
    }

    result.push_str(&fragment("\n  },\n  \"outputs\": {"));
    is_first = true;
    let mut output_paths = HashSet::new();
    for output in output_files {
        if output.json_metadata_chunk.is_empty() {
            continue;
        }
        let path = if options.metafile_path_style == logger::PathStyle::Absolute {
            output.abs_path.clone()
        } else {
            file_system
                .rel(file_system.cwd(), &output.abs_path)
                .unwrap_or_else(|| output.abs_path.clone())
                .replace('\\', "/")
        };
        if !output_paths.insert(path.clone()) {
            continue;
        }
        result.push_str(&fragment(if is_first { "\n    " } else { ",\n    " }));
        is_first = false;
        result.push_str(
            &String::from_utf8(quote_for_json(path.as_bytes(), options.ascii_only))
                .expect("quoted JSON is UTF-8"),
        );
        result.push_str(&fragment(": "));
        result.push_str(&output.json_metadata_chunk);
    }
    result.push_str(&fragment("\n  }\n}"));
    result.push('\n');
    result
}

/// Scan filesystem entry points and compile them as a JavaScript bundle.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn bundle_javascript(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    entry_points: &[EntryPoint],
    options: &mut Options,
    unique_key_prefix: &str,
) -> CompiledBundle {
    let scanned = scan_bundle(
        log,
        file_system,
        caches,
        entry_points,
        options,
        unique_key_prefix,
    );
    if log.has_errors() {
        return CompiledBundle::default();
    }
    compile_javascript_bundle(file_system, &scanned, options, unique_key_prefix)
}

#[derive(Clone, Debug, Default)]
pub struct DataForSourceMap {
    pub line_offset_tables: Vec<LineOffsetTable>,
    pub quoted_contents: Vec<Vec<u8>>,
}

fn compute_source_map_line_offset_tables(files: &[ScannerFile]) -> Vec<Vec<LineOffsetTable>> {
    if files.is_empty() {
        return Vec::new();
    }
    let next_index = std::sync::atomic::AtomicUsize::new(0);
    let results = (0..files.len())
        .map(|_| std::sync::OnceLock::new())
        .collect::<Vec<_>>();
    let worker_count = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(files.len());
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let index = next_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(file) = files.get(index) else {
                        break;
                    };
                    let approximate_line_count = match file.input_file.repr.as_ref() {
                        Some(InputFileRepr::Js(repr)) => repr.ast.approximate_line_count,
                        Some(InputFileRepr::Css(repr)) => repr.ast.approximate_line_count,
                        _ => 0,
                    };
                    let tables = if file.input_file.loader.can_have_source_map()
                        && file.input_file.repr.is_some()
                    {
                        crate::internal::sourcemap::generate_line_offset_tables(
                            &file.input_file.source.contents,
                            approximate_line_count.max(0),
                        )
                    } else {
                        Vec::new()
                    };
                    let did_set = results[index].set(tables).is_ok();
                    assert!(did_set, "source-map data must only be computed once");
                }
            });
        }
    });
    results
        .into_iter()
        .map(|result| {
            result
                .into_inner()
                .expect("every input file must have source-map data")
        })
        .collect()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EntryPoint {
    pub input_path: String,
    pub output_path: String,
    pub input_path_in_file_namespace: bool,
}

fn entry_point_is_file(file_system: &dyn Fs, entry_point: &EntryPoint) -> bool {
    if entry_point.input_path_in_file_namespace {
        return true;
    }
    if entry_point.input_path.contains('*') {
        return false;
    }
    let absolute_path = if file_system.is_abs(&entry_point.input_path) {
        entry_point.input_path.clone()
    } else {
        file_system.join(&[file_system.cwd(), &entry_point.input_path])
    };
    let directory = file_system.dir(&absolute_path);
    let base = file_system.base(&absolute_path);
    let (entries, error, _) = file_system.read_directory(&directory);
    error.is_none()
        && entries
            .get(&base)
            .0
            .is_some_and(|entry| entry.kind(file_system) == EntryKind::File)
}

fn glob_traversal_root(file_system: &dyn Fs, absolute_pattern: &str) -> Option<String> {
    let wildcard = absolute_pattern.find(['*', '?'])?;
    let prefix = &absolute_pattern[..wildcard];
    let trimmed = prefix.trim_end_matches(['/', '\\']);
    Some(if trimmed.is_empty() && prefix.starts_with(['/', '\\']) {
        prefix[..1].to_string()
    } else if trimmed.len() < prefix.len() {
        trimmed.to_string()
    } else {
        file_system.dir(trimmed)
    })
}

fn expand_entry_point_glob(file_system: &dyn Fs, pattern: &str) -> Option<Vec<String>> {
    let absolute_pattern = if file_system.is_abs(pattern) {
        pattern.to_string()
    } else {
        file_system.join(&[file_system.cwd(), pattern])
    };
    let (regexp, had_wildcard) = resolver::globstar_to_escaped_regexp(&absolute_pattern);
    if !had_wildcard {
        return None;
    }
    let regexp = regex::Regex::new(&regexp).ok()?;
    let root = glob_traversal_root(file_system, &absolute_pattern)?;

    fn visit(
        file_system: &dyn Fs,
        directory: &str,
        regexp: &regex::Regex,
        matches: &mut Vec<String>,
    ) {
        let (entries, error, _) = file_system.read_directory(directory);
        if error.is_some() {
            return;
        }
        for name in entries.sorted_keys() {
            let Some(entry) = entries.get(&name).0 else {
                continue;
            };
            let path = file_system.join(&[directory, &name]);
            match entry.kind(file_system) {
                EntryKind::File if regexp.is_match(&path.replace('\\', "/")) => {
                    matches.push(path);
                }
                EntryKind::Dir if entry.symlink(file_system).is_empty() => {
                    visit(file_system, &path, regexp, matches);
                }
                EntryKind::None | EntryKind::Dir | EntryKind::File => {}
            }
        }
    }

    let mut matches = Vec::new();
    visit(file_system, &root, &regexp, &mut matches);
    Some(matches)
}

fn quoted_js_string(text: &str, ascii_only: bool) -> String {
    String::from_utf8(quote_for_json(text.as_bytes(), ascii_only))
        .expect("JSON quoting always produces UTF-8")
}

fn glob_module_path(
    file_system: &dyn Fs,
    options: &Options,
    source: &Source,
    source_directory: &str,
    pattern: &crate::internal::ast::GlobPattern,
    assert_or_with: Option<&crate::internal::ast::ImportAssertOrWith>,
    matches: Vec<String>,
) -> Path {
    let pattern_text = crate::internal::helpers::glob_pattern_to_string(&pattern.parts);
    let call = match pattern.kind {
        ImportKind::Require => "require",
        ImportKind::Dynamic => "import",
        _ => unreachable!("glob imports must be require() or import()"),
    };
    let pretty_call = format!(
        "{call}({})",
        quoted_js_string(&pattern_text, options.ascii_only)
    );
    let mut contents = format!(
        "import {{ __glob }} from \"<runtime>\"; export var {} = __glob({{\n",
        pattern.export_alias
    );
    for absolute_path in matches {
        let Some(mut relative_path) = file_system.rel(source_directory, &absolute_path) else {
            continue;
        };
        relative_path = relative_path.replace('\\', "/");
        if resolver::is_package_path(&relative_path) {
            relative_path = format!("./{relative_path}");
        }
        let quoted = quoted_js_string(&relative_path, options.ascii_only);
        let operation = match pattern.kind {
            ImportKind::Require => "require",
            ImportKind::Dynamic => "import",
            _ => unreachable!(),
        };
        let mut options_suffix = String::new();
        if pattern.kind == ImportKind::Dynamic
            && let Some(assert_or_with) = assert_or_with
        {
            options_suffix.push_str(&format!(",{{{}:{{", assert_or_with.keyword.as_str()));
            for entry in &assert_or_with.entries {
                let key = String::from_utf8_lossy(&utf16_to_string(&entry.key)).into_owned();
                let value = String::from_utf8_lossy(&utf16_to_string(&entry.value)).into_owned();
                options_suffix.push_str(&quoted_js_string(&key, options.ascii_only));
                options_suffix.push(':');
                options_suffix.push_str(&quoted_js_string(&value, options.ascii_only));
                options_suffix.push(',');
            }
            options_suffix.push_str("}}");
        }
        contents.push_str(&format!(
            "{quoted}:()=>{operation}({quoted}{options_suffix}),\n"
        ));
    }
    contents.push_str("});");

    let absolute_pretty = format!("{pretty_call} in {}", source.pretty_paths.abs);
    let relative_pretty = format!("{pretty_call} in {}", source.pretty_paths.rel);
    Path {
        text: format!("{absolute_pretty}\0{relative_pretty}\0{source_directory}\0{contents}"),
        namespace: "glob".into(),
        ..Path::default()
    }
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
    source: Source,
    loader: Loader,
    options: &Options,
    unique_key_prefix: &str,
) -> ParseResult {
    parse_file_with_cache(log, source, loader, options, unique_key_prefix, "", None)
}

#[allow(clippy::too_many_lines)]
fn parse_file_with_cache(
    log: &Log,
    mut source: Source,
    mut loader: Loader,
    options: &Options,
    unique_key_prefix: &str,
    plugin_name: &str,
    caches: Option<&CacheSet>,
) -> ParseResult {
    let (_, base, extension) =
        logger::platform_independent_path_dir_base_ext(&source.key_path.text);
    if loader == Loader::Default {
        loader = crate::internal::config::loader_from_file_extension(
            &options.extension_to_loader,
            &format!("{base}{extension}"),
        );
    }
    if loader != Loader::Copy && plugin_name.is_empty() {
        for attribute in source.key_path.import_attributes.decode_into_array() {
            if attribute.key != "type" {
                log.add_error(
                    None,
                    Range::default(),
                    format!(
                        "Importing with the {:?} attribute is not supported",
                        attribute.key
                    ),
                );
                return ParseResult::default();
            }
            loader = match attribute.value.as_str() {
                "json" => Loader::WithTypeJson,
                "bytes" => Loader::Binary,
                "text" => Loader::Text,
                value => {
                    log.add_error(
                        None,
                        Range::default(),
                        format!("Importing with a type attribute of {value:?} is not supported"),
                    );
                    return ParseResult::default();
                }
            };
        }
    }
    if source.identifier_name.is_empty() && source.key_path.namespace != "glob" {
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
            let (ast, ok) = if let Some(caches) = caches {
                caches.js_cache.parse(log, source.clone(), parser_options)
            } else {
                js_parser::parse(log.clone(), source.clone(), parser_options)
            };
            if ast.parts.len() <= 1
                || ast.parts.iter().all(|part| {
                    part.statements
                        .iter()
                        .all(|statement| statement.data.is_none())
                })
            {
                result.file.input_file.side_effects.kind = SideEffectsKind::NoSideEffectsEmptyAst;
            }
            result.file.input_file.repr = Some(InputFileRepr::Js(Box::new(JsRepr {
                ast,
                ..JsRepr::default()
            })));
            result.ok = ok;
        }
        Loader::Css | Loader::GlobalCss | Loader::LocalCss => {
            let parser_options = css_parser::Options {
                minify_syntax: options.minify_syntax,
                minify_whitespace: options.minify_whitespace,
                minify_identifiers: options.minify_identifiers,
                unsupported_css_features: options.unsupported_css_features,
                css_prefix_data: options.css_prefix_data.clone(),
                symbol_mode: match loader {
                    Loader::LocalCss => css_parser::SymbolMode::Local,
                    Loader::GlobalCss => css_parser::SymbolMode::Global,
                    _ => css_parser::SymbolMode::Disabled,
                },
            };
            let ast = if let Some(caches) = caches {
                caches.css_cache.parse(log, source.clone(), parser_options)
            } else {
                css_parser::parse(log.clone(), source.clone(), parser_options)
            };
            result.file.input_file.repr = Some(InputFileRepr::Css(Box::new(CssRepr {
                ast,
                ..CssRepr::default()
            })));
            result.ok = true;
        }
        Loader::Json | Loader::WithTypeJson => {
            let parser_options = js_parser::JsonOptions {
                unsupported_js_features: options.unsupported_js_features,
                ..js_parser::JsonOptions::default()
            };
            let (expression, ok) = if let Some(caches) = caches {
                caches.json_cache.parse(log, source.clone(), parser_options)
            } else {
                js_parser::parse_json(log.clone(), source.clone(), parser_options)
            };
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

    if !plugin_name.is_empty()
        && result.file.input_file.side_effects.kind == SideEffectsKind::NoSideEffectsPureData
    {
        result.file.input_file.side_effects.kind = SideEffectsKind::NoSideEffectsPureDataFromPlugin;
    }
    result
}

fn identifier_name_from_path(
    file_system: &dyn Fs,
    options: &Options,
    absolute_path: &str,
) -> String {
    if let Some(mut relative) = file_system.rel(&options.abs_output_base, absolute_path) {
        loop {
            let next = relative
                .strip_prefix("../")
                .or_else(|| relative.strip_prefix("..\\"));
            let Some(next) = next else {
                break;
            };
            relative = next.to_string();
        }
        return js_ast::generate_non_unique_name_from_path(&relative);
    }
    js_ast::generate_non_unique_name_from_path(absolute_path)
}

fn lowest_common_ancestor_directory(file_system: &dyn Fs, absolute_paths: &[String]) -> String {
    let Some(first) = absolute_paths.first() else {
        return String::new();
    };
    let mut lowest_abs_dir = file_system.dir(first);

    for absolute_path in &absolute_paths[1..] {
        let abs_dir = file_system.dir(absolute_path);
        let mut last_slash = 0;
        let mut a = 0;
        let mut b = 0;

        loop {
            let rune_a = abs_dir[a..].chars().next();
            let rune_b = lowest_abs_dir[b..].chars().next();
            let boundary_a = rune_a.is_none_or(|c| matches!(c, '/' | '\\'));
            let boundary_b = rune_b.is_none_or(|c| matches!(c, '/' | '\\'));

            if boundary_a && boundary_b {
                if rune_a.is_none() || rune_b.is_none() {
                    lowest_abs_dir.truncate(a);
                    break;
                }
                last_slash = a;
            } else if boundary_a != boundary_b
                || matches!((rune_a, rune_b), (Some(a), Some(b)) if !a.to_lowercase().eq(b.to_lowercase()))
            {
                if last_slash < abs_dir.len() && !abs_dir[..last_slash].contains(['/', '\\']) {
                    last_slash += 1;
                }
                lowest_abs_dir = abs_dir[..last_slash].to_string();
                break;
            }

            a += rune_a.map_or(0, char::len_utf8);
            b += rune_b.map_or(0, char::len_utf8);
        }
    }

    lowest_abs_dir
}

fn sanitize_plugin_location(file_system: &dyn Fs, location: &mut logger::MsgLocation) {
    if location.namespace.is_empty() {
        location.namespace = "file".into();
    }
    if location.file == logger::PrettyPaths::default() {
        return;
    }
    let absolute_path = location.file.abs.clone();
    let relative_path = if location.namespace == "file" {
        file_system
            .rel(file_system.cwd(), &absolute_path)
            .unwrap_or_else(|| absolute_path.clone())
            .replace('\\', "/")
    } else {
        format!("{}:{absolute_path}", location.namespace)
    };
    location.file = logger::PrettyPaths {
        abs: if location.namespace == "file" {
            absolute_path
        } else {
            relative_path.clone()
        },
        rel: relative_path,
    };
}

fn log_plugin_messages(
    log: &Log,
    file_system: &dyn Fs,
    default_plugin_name: &str,
    messages: Vec<Msg>,
    thrown_error: Option<String>,
    import_source: Option<&Source>,
    import_path_range: Range,
) -> bool {
    let mut did_log_error = false;
    let mut tracker = LineColumnTracker::new(import_source);
    for mut message in messages {
        if message.plugin_name.is_empty() {
            message.plugin_name = default_plugin_name.to_string();
        }
        for note in &mut message.notes {
            if let Some(location) = &mut note.location {
                sanitize_plugin_location(file_system, location);
            }
        }
        let had_explicit_location = message.data.location.is_some();
        if message.data.location.is_none() {
            message.data.location = tracker.msg_location_or_none(import_path_range);
        } else if let Some(location) = &mut message.data.location {
            sanitize_plugin_location(file_system, location);
            if location.file == logger::PrettyPaths::default()
                && let Some(import_source) = import_source
            {
                location.file.clone_from(&import_source.pretty_paths);
            }
        }
        if had_explicit_location && import_source.is_some() {
            message.notes.push(tracker.msg_data(
                import_path_range,
                format!("The plugin {default_plugin_name:?} was triggered by this import"),
            ));
        }
        did_log_error |= message.kind == MsgKind::Error;
        log.add_msg(message);
    }
    if let Some(text) = thrown_error {
        let mut message = Msg {
            data: tracker.msg_data(import_path_range, text),
            ..Msg::new(MsgKind::Error, "")
        };
        message.plugin_name = default_plugin_name.to_string();
        log.add_msg(message);
        did_log_error = true;
    }
    did_log_error
}

fn touch_plugin_watch_paths(
    file_system: &dyn Fs,
    caches: &CacheSet,
    watch_files: &[String],
    watch_dirs: &[String],
) {
    for path in watch_files {
        let _ = caches.fs_cache.read_file(file_system, path);
    }
    for path in watch_dirs {
        let (entries, error, _) = file_system.read_directory(path);
        if error.is_none() {
            let _ = entries.sorted_keys();
        }
    }
}

pub(crate) fn run_on_start_plugins(log: &Log, file_system: &dyn Fs, plugins: &[config::Plugin]) {
    std::thread::scope(|scope| {
        for plugin in plugins {
            for on_start in &plugin.on_start {
                let Some(callback) = on_start.callback.clone() else {
                    continue;
                };
                let plugin_name = plugin.name.clone();
                scope.spawn(move || {
                    let mut result =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()))
                        {
                            Ok(result) => result,
                            Err(_) => config::OnStartResult {
                                thrown_error: Some("Plugin onStart callback panicked".into()),
                                ..config::OnStartResult::default()
                            },
                        };
                    log_plugin_messages(
                        log,
                        file_system,
                        &plugin_name,
                        std::mem::take(&mut result.messages),
                        result.thrown_error.take(),
                        None,
                        Range::default(),
                    );
                });
            }
        }
    });
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn resolve_with_plugins(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    options: &Options,
    importer: &Path,
    path: &str,
    import_attributes: &logger::ImportAttributes,
    kind: ImportKind,
    abs_resolve_dir: &str,
    plugin_data: Option<PluginData>,
    import_source: Option<&Source>,
    import_path_range: Range,
    tsconfig: Option<&resolver::TsConfigJson>,
    is_require: bool,
) -> (Option<ResolveResult>, bool) {
    let apply_path = Path {
        text: path.to_string(),
        namespace: importer.namespace.clone(),
        ..Path::default()
    };
    for plugin in &options.plugins {
        for on_resolve in &plugin.on_resolve {
            let (Some(filter), Some(callback)) = (&on_resolve.filter, &on_resolve.callback) else {
                continue;
            };
            if !config::plugin_applies_to_path(&apply_path, filter, &on_resolve.namespace) {
                continue;
            }
            let args = config::OnResolveArgs {
                path: path.to_string(),
                resolve_dir: abs_resolve_dir.to_string(),
                plugin_data: plugin_data.clone(),
                importer: importer.clone(),
                kind,
                with: import_attributes.clone(),
            };
            let mut plugin_result =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(args))) {
                    Ok(result) => result,
                    Err(_) => config::OnResolveResult {
                        thrown_error: Some("Plugin onResolve callback panicked".into()),
                        ..config::OnResolveResult::default()
                    },
                };
            let plugin_name = if plugin_result.plugin_name.is_empty() {
                plugin.name.clone()
            } else {
                plugin_result.plugin_name.clone()
            };
            touch_plugin_watch_paths(
                file_system,
                caches,
                &plugin_result.abs_watch_files,
                &plugin_result.abs_watch_dirs,
            );
            if log_plugin_messages(
                log,
                file_system,
                &plugin_name,
                std::mem::take(&mut plugin_result.messages),
                plugin_result.thrown_error.take(),
                import_source,
                import_path_range,
            ) {
                return (None, true);
            }

            let namespace_from_plugin = plugin_result.path.namespace.clone();
            if plugin_result.path.namespace.is_empty() && !plugin_result.external {
                plugin_result.path.namespace = "file".into();
            }
            if plugin_result.path.text.is_empty() {
                if plugin_result.external {
                    plugin_result.path = Path {
                        text: path.to_string(),
                        ..Path::default()
                    };
                } else {
                    continue;
                }
            }
            if plugin_result.path.namespace == "file"
                && !file_system.is_abs(&plugin_result.path.text)
            {
                let text = if namespace_from_plugin == "file" {
                    format!(
                        "Plugin {plugin_name:?} returned a path in the \"file\" namespace that is not an absolute path: {}",
                        plugin_result.path.text
                    )
                } else {
                    format!(
                        "Plugin {plugin_name:?} returned a non-absolute path: {} (set a namespace if this is not a file path)",
                        plugin_result.path.text
                    )
                };
                let mut tracker = LineColumnTracker::new(import_source);
                log.add_error(Some(&mut tracker), import_path_range, text);
                return (None, true);
            }
            plugin_result
                .path
                .import_attributes
                .clone_from(import_attributes);
            let side_effects_data =
                plugin_result
                    .is_side_effect_free
                    .then(|| resolver::SideEffectsData {
                        plugin_name,
                        ..resolver::SideEffectsData::default()
                    });
            return (
                Some(ResolveResult {
                    path_pair: resolver::PathPair {
                        primary: plugin_result.path,
                        is_external: plugin_result.external,
                        ..resolver::PathPair::default()
                    },
                    plugin_data: plugin_result.plugin_data,
                    primary_side_effects_data: side_effects_data,
                    ..ResolveResult::default()
                }),
                false,
            );
        }
    }

    let mut result = resolver::resolve_with_metadata(
        log,
        file_system,
        abs_resolve_dir,
        path,
        &options.extension_order,
        options.platform,
        (!options.main_fields.is_empty()).then_some(options.main_fields.as_slice()),
        is_require,
        ResolverContext {
            import_kind: kind,
            tsconfig,
            external_settings: Some(&options.external_settings),
            external_packages: options.external_packages,
            preserve_symlinks: options.preserve_symlinks,
            conditions: Some(&options.conditions),
            package_aliases: Some(&options.package_aliases),
            node_paths: Some(&options.abs_node_paths),
            ..ResolverContext::default()
        },
    );
    if let Some(result) = &mut result {
        let is_external = result.path_pair.is_external;
        for path in result.path_pair.iter_mut() {
            if path.namespace.is_empty() && !is_external {
                path.namespace = "file".into();
            }
            path.import_attributes.clone_from(import_attributes);
        }
    }
    (result, false)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_for_plugin_api(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    options: &Options,
    importer: &Path,
    path: &str,
    import_attributes: &logger::ImportAttributes,
    kind: ImportKind,
    abs_resolve_dir: &str,
    plugin_data: Option<PluginData>,
    raw_tsconfig: Option<&resolver::TsConfigJson>,
) -> (Option<ResolveResult>, bool) {
    let discovered_tsconfig =
        if raw_tsconfig.is_none() && options.tsconfig_raw.is_empty() && !abs_resolve_dir.is_empty()
        {
            find_nearest_tsconfig(
                log,
                file_system,
                abs_resolve_dir,
                (!options.tsconfig_path.is_empty()).then_some(options.tsconfig_path.as_str()),
            )
        } else {
            None
        };
    resolve_with_plugins(
        log,
        file_system,
        caches,
        options,
        importer,
        path,
        import_attributes,
        kind,
        abs_resolve_dir,
        plugin_data,
        None,
        Range::default(),
        raw_tsconfig.or(discovered_tsconfig.as_ref()),
        matches!(kind, ImportKind::Require | ImportKind::RequireResolve),
    )
}

struct LoadedFile {
    loader: Loader,
    abs_resolve_dir: String,
    plugin_name: String,
    plugin_data: Option<PluginData>,
}

#[derive(Clone)]
struct PendingFile {
    path: Path,
    source_index: u32,
    resolve_metadata: ResolveResult,
    import_source: Option<Source>,
    import_path_range: Range,
}

fn enqueue_dependencies(
    result: &ParseResult,
    caches: &CacheSet,
    queued: &mut HashSet<u32>,
    pending: &mut Vec<PendingFile>,
) {
    let records = result
        .file
        .input_file
        .repr
        .as_ref()
        .and_then(InputFileRepr::import_records);
    for (record_index, resolve_result) in result.resolve_results.iter().enumerate() {
        let Some(resolve_result) = resolve_result else {
            continue;
        };
        if resolve_result.path_pair.is_external {
            continue;
        }
        let dependency_path = &resolve_result.path_pair.primary;
        let dependency_index = caches
            .source_index_cache
            .get(dependency_path.clone(), SourceIndexKind::Normal);
        if queued.insert(dependency_index) {
            pending.push(PendingFile {
                path: dependency_path.clone(),
                source_index: dependency_index,
                resolve_metadata: resolve_result.clone(),
                import_source: Some(result.file.input_file.source.clone()),
                import_path_range: records
                    .and_then(|records| records.get(record_index))
                    .map_or_else(Range::default, |record| record.range),
            });
        }
    }
}

fn warn_about_ignored_bare_imports(log: &Log, options: &Options, files: &[ScannerFile]) {
    if options.ignore_dce_annotations {
        return;
    }
    for file in files {
        let importer = &file.input_file.source;
        if importer.key_path.text.is_empty()
            || crate::internal::helpers::is_inside_node_modules(&importer.key_path.text)
        {
            continue;
        }
        let Some(records) = file
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
        else {
            continue;
        };
        let mut tracker = LineColumnTracker::new(Some(importer));
        for record in records {
            if !record
                .flags
                .contains(ImportRecordFlags::WAS_ORIGINALLY_BARE_IMPORT)
                || !record.source_index.is_valid()
            {
                continue;
            }
            let Some(target) = files.get(record.source_index.get_index() as usize) else {
                continue;
            };
            if matches!(
                target.input_file.side_effects.kind,
                SideEffectsKind::HasSideEffects
                    | SideEffectsKind::NoSideEffectsPureDataFromPlugin
                    | SideEffectsKind::NoSideEffectsEmptyAst
            ) {
                continue;
            }
            let mut notes = Vec::new();
            let mut by = String::new();
            if let Some(data) = &target.input_file.side_effects.data {
                if !data.plugin_name.is_empty() {
                    by = format!(" by plugin {:?}", data.plugin_name);
                } else if let Some(source) = &data.source {
                    let text = if data.is_side_effects_array_in_json {
                        "It was excluded from the \"sideEffects\" array in the enclosing \"package.json\" file:"
                    } else {
                        "\"sideEffects\" is false in the enclosing \"package.json\" file:"
                    };
                    notes.push(LineColumnTracker::new(Some(source)).msg_data(data.range, text));
                }
            }
            log.add_id_with_notes(
                logger::MsgId::BundlerIgnoredBareImport,
                MsgKind::Warning,
                Some(&mut tracker),
                record.range,
                format!(
                    "Ignoring this import because {:?} was marked as having no side effects{by}",
                    target
                        .input_file
                        .source
                        .pretty_paths
                        .select(options.log_path_style)
                ),
                notes,
            );
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn load_file_with_plugins(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    options: &Options,
    source: &mut Source,
    plugin_data: Option<PluginData>,
    import_source: Option<&Source>,
    import_path_range: Range,
) -> Option<LoadedFile> {
    let args = config::OnLoadArgs {
        plugin_data,
        path: source.key_path.clone(),
    };
    for plugin in &options.plugins {
        for on_load in &plugin.on_load {
            let (Some(filter), Some(callback)) = (&on_load.filter, &on_load.callback) else {
                continue;
            };
            if !config::plugin_applies_to_path(&source.key_path, filter, &on_load.namespace) {
                continue;
            }
            let mut plugin_result =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    callback(args.clone())
                })) {
                    Ok(result) => result,
                    Err(_) => config::OnLoadResult {
                        thrown_error: Some("Plugin onLoad callback panicked".into()),
                        ..config::OnLoadResult::default()
                    },
                };
            let plugin_name = if plugin_result.plugin_name.is_empty() {
                plugin.name.clone()
            } else {
                plugin_result.plugin_name.clone()
            };
            touch_plugin_watch_paths(
                file_system,
                caches,
                &plugin_result.abs_watch_files,
                &plugin_result.abs_watch_dirs,
            );
            if log_plugin_messages(
                log,
                file_system,
                &plugin_name,
                std::mem::take(&mut plugin_result.messages),
                plugin_result.thrown_error.take(),
                import_source,
                import_path_range,
            ) {
                if options.watch_mode && source.key_path.namespace == "file" {
                    let _ = caches
                        .fs_cache
                        .read_file(file_system, &source.key_path.text);
                }
                return None;
            }
            let Some(contents) = plugin_result.contents else {
                continue;
            };
            source.contents = Arc::from(contents.into_bytes());
            let loader = if plugin_result.loader == Loader::None {
                Loader::Js
            } else {
                plugin_result.loader
            };
            if plugin_result.abs_resolve_dir.is_empty() && source.key_path.namespace == "file" {
                plugin_result.abs_resolve_dir = file_system.dir(&source.key_path.text);
            }
            if options.watch_mode && source.key_path.namespace == "file" {
                let _ = caches
                    .fs_cache
                    .read_file(file_system, &source.key_path.text);
            }
            return Some(LoadedFile {
                loader,
                abs_resolve_dir: plugin_result.abs_resolve_dir,
                plugin_name,
                plugin_data: plugin_result.plugin_data,
            });
        }
    }

    if source.key_path.namespace == "glob" {
        let mut parts = source.key_path.text.splitn(4, '\0');
        let _absolute_pretty = parts.next()?;
        let _relative_pretty = parts.next()?;
        let abs_resolve_dir = parts.next()?.to_string();
        source.contents = Arc::from(parts.next()?.as_bytes());
        return Some(LoadedFile {
            loader: Loader::Js,
            abs_resolve_dir,
            plugin_name: String::new(),
            plugin_data: None,
        });
    }

    if source.key_path.is_disabled() {
        return Some(LoadedFile {
            loader: Loader::Empty,
            abs_resolve_dir: String::new(),
            plugin_name: String::new(),
            plugin_data: None,
        });
    }
    if source.key_path.namespace == "file" {
        let (contents, error, _) = caches
            .fs_cache
            .read_file(file_system, &source.key_path.text);
        if let Some(error) = error {
            let mut tracker = LineColumnTracker::new(import_source);
            log.add_error(
                Some(&mut tracker),
                import_path_range,
                format!(
                    "Could not read from file {:?}: {}",
                    source.key_path.text, error.message
                ),
            );
            return None;
        }
        source.contents = Arc::from(contents);
        return Some(LoadedFile {
            loader: Loader::Default,
            abs_resolve_dir: file_system.dir(&source.key_path.text),
            plugin_name: String::new(),
            plugin_data: None,
        });
    }
    if source.key_path.namespace == "dataurl"
        && let Some(data_url) = resolver::DataUrl::parse(&source.key_path.text)
    {
        match data_url.decode_data() {
            Ok(contents) => source.contents = Arc::from(contents),
            Err(error) => {
                let mut tracker = LineColumnTracker::new(import_source);
                log.add_error(
                    Some(&mut tracker),
                    import_path_range,
                    format!("Could not load data URL: {error}"),
                );
                return None;
            }
        }
        let loader = match data_url.decode_mime_type() {
            resolver::MimeType::TextCss => Loader::Css,
            resolver::MimeType::TextJavaScript => Loader::Js,
            resolver::MimeType::ApplicationJson => Loader::Json,
            resolver::MimeType::Unsupported => Loader::None,
        };
        return Some(LoadedFile {
            loader,
            abs_resolve_dir: String::new(),
            plugin_name: String::new(),
            plugin_data: None,
        });
    }
    Some(LoadedFile {
        loader: Loader::None,
        abs_resolve_dir: String::new(),
        plugin_name: String::new(),
        plugin_data: None,
    })
}

fn rewrite_external_path(file_system: &dyn Fs, options: &Options, mut path: Path) -> Path {
    if path.namespace == "file"
        && let Some(mut relative_path) = file_system.rel(&options.abs_output_dir, &path.text)
    {
        relative_path = relative_path.replace('\\', "/");
        if resolver::is_package_path(&relative_path) {
            relative_path = format!("./{relative_path}");
        }
        path.text = relative_path;
    }
    path
}

pub fn resolve_import_records(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    options: &Options,
    tsconfig: Option<&resolver::TsConfigJson>,
    result: &mut ParseResult,
) {
    resolve_import_records_from_directory(
        log,
        file_system,
        caches,
        options,
        tsconfig,
        None,
        result,
    );
}

#[allow(clippy::too_many_lines)]
fn resolve_import_records_from_directory(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    options: &Options,
    tsconfig: Option<&resolver::TsConfigJson>,
    source_directory: Option<&str>,
    result: &mut ParseResult,
) {
    if options.mode != Mode::Bundle || !result.ok {
        return;
    }
    let source = result.file.input_file.source.clone();
    let plugin_data = result.file.plugin_data.clone();
    let source_directory =
        source_directory.map_or_else(|| file_system.dir(&source.key_path.text), str::to_string);
    let Some(records) = result
        .file
        .input_file
        .repr
        .as_mut()
        .and_then(InputFileRepr::import_records_mut)
    else {
        return;
    };
    result.resolve_results = vec![None; records.len()];
    let mut tracker = LineColumnTracker::new(Some(&source));
    let mut resolution_cache: HashMap<
        (ImportKind, String, logger::ImportAttributes),
        (Option<ResolveResult>, bool),
    > = HashMap::new();

    for (record_index, record) in records.iter_mut().enumerate() {
        if record.source_index.is_valid() || record.flags.contains(ImportRecordFlags::IS_UNUSED) {
            continue;
        }
        if source.key_path.namespace == "glob" && record.path.text == "<runtime>" {
            record.source_index = Index32::new(runtime::SOURCE_INDEX);
            continue;
        }
        if let Some(pattern) = &record.glob_pattern {
            let pattern_text = crate::internal::helpers::glob_pattern_to_string(&pattern.parts);
            let call = match pattern.kind {
                ImportKind::Require => "require",
                ImportKind::Dynamic => "import",
                _ => unreachable!("glob imports must be require() or import()"),
            };
            let pretty_call = format!(
                "{call}({})",
                quoted_js_string(&pattern_text, options.ascii_only)
            );
            let absolute_pattern = file_system.join(&[&source_directory, &pattern_text]);
            let Some(root) = glob_traversal_root(file_system, &absolute_pattern) else {
                continue;
            };
            let (_, root_error, _) = file_system.read_directory(&root);
            if root_error.is_some() {
                log.add_error(
                    Some(&mut tracker),
                    record.range,
                    format!("Could not resolve {pretty_call}"),
                );
                continue;
            }
            let matches =
                expand_entry_point_glob(file_system, &absolute_pattern).unwrap_or_default();
            if matches.is_empty() {
                log.add_id(
                    logger::MsgId::BundlerEmptyGlob,
                    MsgKind::Warning,
                    Some(&mut tracker),
                    record.range,
                    format!("The glob pattern {pretty_call} did not match any files"),
                );
            }
            let path = glob_module_path(
                file_system,
                options,
                &source,
                &source_directory,
                pattern,
                record.assert_or_with.as_ref(),
                matches,
            );
            record.source_index = Index32::new(
                caches
                    .source_index_cache
                    .get(path.clone(), SourceIndexKind::Normal),
            );
            result.resolve_results[record_index] = Some(ResolveResult {
                path_pair: resolver::PathPair {
                    primary: path,
                    ..resolver::PathPair::default()
                },
                ..ResolveResult::default()
            });
            continue;
        }
        let is_require = matches!(
            record.kind,
            ImportKind::Require | ImportKind::RequireResolve
        );
        let import_attributes = record
            .assert_or_with
            .as_ref()
            .filter(|clause| clause.keyword == AssertOrWithKeyword::With)
            .map(|clause| {
                logger::ImportAttributes::encode(
                    &clause
                        .entries
                        .iter()
                        .map(|entry| {
                            (
                                String::from_utf8_lossy(&utf16_to_string(&entry.key)).into_owned(),
                                String::from_utf8_lossy(&utf16_to_string(&entry.value))
                                    .into_owned(),
                            )
                        })
                        .collect(),
                )
            })
            .unwrap_or_default();
        let cache_key = (
            record.kind,
            record.path.text.clone(),
            import_attributes.clone(),
        );
        let (resolve_result, did_log_plugin_error) =
            if let Some(cached) = resolution_cache.get(&cache_key) {
                cached.clone()
            } else {
                let resolved = resolve_with_plugins(
                    log,
                    file_system,
                    caches,
                    options,
                    &source.key_path,
                    &record.path.text,
                    &import_attributes,
                    record.kind,
                    &source_directory,
                    plugin_data.clone(),
                    Some(&source),
                    record.range,
                    tsconfig,
                    is_require,
                );
                resolution_cache.insert(cache_key.clone(), resolved.clone());
                resolved
            };
        let Some(resolve_result) = resolve_result else {
            if !did_log_plugin_error
                && !record
                    .flags
                    .contains(ImportRecordFlags::HANDLES_IMPORT_ERRORS)
            {
                log.add_error(
                    Some(&mut tracker),
                    record.range,
                    format!("Could not resolve {:?}", record.path.text),
                );
                if let Some(cached) = resolution_cache.get_mut(&cache_key) {
                    cached.1 = true;
                }
            }
            continue;
        };
        if record.kind == ImportKind::RequireResolve {
            if resolve_result.path_pair.is_external {
                record.path = rewrite_external_path(
                    file_system,
                    options,
                    resolve_result.path_pair.primary.clone(),
                );
                if resolve_result.primary_side_effects_data.is_some() {
                    record.flags |= ImportRecordFlags::IS_EXTERNAL_WITHOUT_SIDE_EFFECTS;
                }
                result.resolve_results[record_index] = Some(resolve_result);
            }
            continue;
        }
        if resolve_result.path_pair.is_external {
            record.path = rewrite_external_path(
                file_system,
                options,
                resolve_result.path_pair.primary.clone(),
            );
            if resolve_result.primary_side_effects_data.is_some() {
                record.flags |= ImportRecordFlags::IS_EXTERNAL_WITHOUT_SIDE_EFFECTS;
            }
        } else {
            record.source_index = Index32::new(caches.source_index_cache.get(
                resolve_result.path_pair.primary.clone(),
                SourceIndexKind::Normal,
            ));
        }
        result.resolve_results[record_index] = Some(resolve_result);
    }
}

fn find_nearest_tsconfig(
    log: &Log,
    file_system: &dyn Fs,
    start_directory: &str,
    override_path: Option<&str>,
) -> Option<resolver::TsConfigJson> {
    fn discover_pnp(
        log: &Log,
        file_system: &dyn Fs,
        start_directory: &str,
    ) -> Option<resolver::PnpData> {
        let mut directory = start_directory.to_string();
        loop {
            let path = file_system.join(&[&directory, ".pnp.data.json"]);
            let (contents, error, _) = file_system.read_file(&path);
            if error.is_none() {
                let source = Source {
                    key_path: Path {
                        text: path.clone(),
                        namespace: "file".into(),
                        ..Path::default()
                    },
                    contents: Arc::from(contents),
                    ..Source::default()
                };
                let (json, ok) =
                    js_parser::parse_json(log.clone(), source, js_parser::JsonOptions::default());
                if ok {
                    return Some(resolver::compile_yarn_pnp_data(&path, &directory, &json));
                }
                return None;
            }
            let parent = file_system.dir(&directory);
            if parent.is_empty() || parent == directory {
                return None;
            }
            directory = parent;
        }
    }

    fn load(
        log: &Log,
        file_system: &dyn Fs,
        path: &str,
        visited: &mut HashSet<String>,
        config_dir: &str,
        pnp: Option<&resolver::PnpData>,
    ) -> Option<resolver::TsConfigJson> {
        if !visited.insert(path.to_string()) {
            return None;
        }
        let (contents, error, _) = file_system.read_file(path);
        if error.is_some() {
            return None;
        }
        let directory = file_system.dir(path);
        let source = Source {
            key_path: Path {
                text: path.to_string(),
                namespace: "file".into(),
                ..Path::default()
            },
            pretty_paths: logger::PrettyPaths {
                abs: path.to_string(),
                rel: file_system
                    .rel(file_system.cwd(), path)
                    .unwrap_or_else(|| path.to_string()),
            },
            contents: Arc::from(contents),
            ..Source::default()
        };
        let mut tracker = LineColumnTracker::new(Some(&source));
        let mut extends = |text: &str, range: Range| {
            let mut did_find_candidate = false;
            let mut try_file = |path: &str, visited: &mut HashSet<String>| {
                let (_, error, _) = file_system.read_file(path);
                if error.is_none() {
                    did_find_candidate = true;
                    if visited.contains(path) {
                        log.add_id(
                            logger::MsgId::TsConfigJsonCycle,
                            MsgKind::Warning,
                            Some(&mut tracker),
                            range,
                            format!("Base config file {text:?} forms cycle"),
                        );
                        None
                    } else {
                        load(log, file_system, path, visited, config_dir, pnp)
                    }
                } else {
                    None
                }
            };

            if !file_system.is_abs(text) && !text.starts_with('.') {
                if let Some(resolved) = resolver::resolve_file_or_package_with_context(
                    log,
                    file_system,
                    &directory,
                    text,
                    &[],
                    Platform::Neutral,
                    None,
                    false,
                    ResolverContext {
                        pnp,
                        ..ResolverContext::default()
                    },
                ) && let Some(config) = try_file(&resolved.paths.primary.text, visited)
                {
                    return Some(config);
                }
                let (package_name, _) = resolver::parse_esm_package_name(text)?;
                let mut current = directory.clone();
                loop {
                    if file_system.base(&current) != "node_modules" {
                        let package_dir =
                            file_system.join(&[&current, "node_modules", package_name]);
                        let mut joined = file_system.join(&[&current, "node_modules", text]);
                        let package_path = file_system.join(&[&package_dir, "package.json"]);
                        let (package_contents, package_error, _) =
                            file_system.read_file(&package_path);
                        if package_error.is_none() {
                            let package_source = Source {
                                key_path: Path {
                                    text: package_path,
                                    namespace: "file".into(),
                                    ..Path::default()
                                },
                                contents: Arc::from(package_contents),
                                ..Source::default()
                            };
                            if let Some(package) = resolver::parse_package_json(
                                log,
                                &package_source,
                                &package_dir,
                                file_system,
                                Platform::Neutral,
                                None,
                            ) && !package.tsconfig.is_empty()
                            {
                                joined = if file_system.is_abs(&package.tsconfig) {
                                    package.tsconfig
                                } else {
                                    file_system.join(&[&package_dir, &package.tsconfig])
                                };
                            }
                        }

                        for candidate in [
                            file_system.join(&[&joined, "tsconfig.json"]),
                            joined.clone(),
                            format!("{joined}.json"),
                        ] {
                            if let Some(config) = try_file(&candidate, visited) {
                                return Some(config);
                            }
                        }
                    }
                    let parent = file_system.dir(&current);
                    if parent.is_empty() || parent == current {
                        break;
                    }
                    current = parent;
                }
                drop(try_file);
                if !did_find_candidate
                    && !crate::internal::helpers::is_inside_node_modules(&source.key_path.text)
                {
                    log.add_id(
                        logger::MsgId::TsConfigJsonMissing,
                        MsgKind::Warning,
                        Some(&mut tracker),
                        range,
                        format!("Cannot find base config file {text:?}"),
                    );
                }
                return None;
            }

            let mut extended = if file_system.is_abs(text) {
                text.to_string()
            } else {
                file_system.join(&[&directory, text])
            };
            if matches!(text, "." | "..") {
                extended = file_system.join(&[&extended, "tsconfig.json"]);
            }
            if let Some(config) = try_file(&extended, visited) {
                return Some(config);
            }
            if !extended.ends_with(".json") {
                if let Some(config) = try_file(&format!("{extended}.json"), visited) {
                    return Some(config);
                }
            }
            drop(try_file);
            if !did_find_candidate
                && !crate::internal::helpers::is_inside_node_modules(&source.key_path.text)
            {
                log.add_id(
                    logger::MsgId::TsConfigJsonMissing,
                    MsgKind::Warning,
                    Some(&mut tracker),
                    range,
                    format!("Cannot find base config file {text:?}"),
                );
            }
            None
        };
        let result = resolver::parse_tsconfig_json(
            log,
            &source,
            file_system,
            &directory,
            config_dir,
            Some(&mut extends),
        );
        visited.remove(path);
        result
    }

    let mut visited = HashSet::new();
    let pnp = discover_pnp(log, file_system, start_directory);
    if let Some(path) = override_path {
        let (_, error, _) = file_system.read_file(path);
        if error.is_some() {
            log.add_error(
                None,
                Range::default(),
                format!("Cannot find tsconfig file {path:?}"),
            );
            return None;
        }
        let config_dir = file_system.dir(path);
        return load(
            log,
            file_system,
            path,
            &mut visited,
            &config_dir,
            pnp.as_ref(),
        );
    }
    if crate::internal::helpers::is_inside_node_modules(start_directory) {
        return None;
    }
    let mut directory = start_directory.to_string();
    loop {
        for config_name in ["tsconfig.json", "jsconfig.json"] {
            let path = file_system.join(&[&directory, config_name]);
            let (_, error, _) = file_system.read_file(&path);
            if error.is_none() {
                return load(
                    log,
                    file_system,
                    &path,
                    &mut visited,
                    &directory,
                    pnp.as_ref(),
                );
            }
        }
        let parent = file_system.dir(&directory);
        if parent.is_empty() || parent == directory {
            return None;
        }
        directory = parent;
    }
}

/// Scan entry points and their recursively resolved dependencies into a graph.
///
/// # Panics
///
/// Panics if a source index cannot fit into the host's address space.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
pub fn scan_bundle(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    entry_points: &[EntryPoint],
    options: &mut Options,
    unique_key_prefix: &str,
) -> ScannedBundle {
    apply_option_defaults(options);
    run_on_start_plugins(log, file_system, &options.plugins);
    let mut bundle = ScannedBundle::default();

    let runtime_source = runtime::source(options.unsupported_js_features);
    let runtime_options = Options {
        defines: Some(Arc::new(config::process_defines(&[]))),
        unsupported_js_features: options.unsupported_js_features,
        minify_syntax: options.minify_syntax,
        minify_identifiers: options.minify_identifiers,
        // The runtime is a library of helpers. It must always be tree-shaken
        // independently of whether tree shaking is enabled for user code.
        tree_shaking: true,
        ..Options::default()
    };
    let runtime_result = parse_file_with_cache(
        log,
        runtime_source,
        Loader::Js,
        &runtime_options,
        unique_key_prefix,
        "",
        Some(caches),
    );
    if runtime_result.ok {
        let mut runtime_file = runtime_result.file;
        runtime_file.input_file.omit_from_source_maps_and_metafile = true;
        bundle.files.push(runtime_file);
    } else {
        bundle.files.push(ScannerFile::default());
    }

    let mut pending = Vec::new();
    let mut queued = HashSet::from([runtime::SOURCE_INDEX]);
    let mut resolution_slots: HashMap<u32, Vec<Option<ResolveResult>>> = HashMap::new();
    if options.abs_output_base.is_empty() && entry_points.is_empty() {
        options.abs_output_base = file_system.cwd().to_string();
    }
    if let Some(stdin) = options.stdin.clone() {
        let key_path = if stdin.source_file.is_empty() {
            Path {
                text: "<stdin>".into(),
                ..Path::default()
            }
        } else if stdin.abs_resolve_dir.is_empty() {
            Path {
                text: stdin.source_file.clone(),
                ..Path::default()
            }
        } else {
            Path {
                text: if file_system.is_abs(&stdin.source_file) {
                    stdin.source_file.clone()
                } else {
                    file_system.join(&[&stdin.abs_resolve_dir, &stdin.source_file])
                },
                namespace: "file".into(),
                ..Path::default()
            }
        };
        let source_index = caches
            .source_index_cache
            .get(key_path.clone(), SourceIndexKind::Normal);
        let pretty_path = if stdin.source_file.is_empty() {
            "<stdin>".to_string()
        } else {
            stdin.source_file.clone()
        };
        let source = Source {
            index: source_index,
            key_path,
            pretty_paths: logger::PrettyPaths {
                abs: pretty_path.clone(),
                rel: pretty_path,
            },
            contents: Arc::from(stdin.contents.into_bytes()),
            ..Source::default()
        };
        let mut file_options = options.clone();
        let tsconfig = if options.tsconfig_raw.is_empty() {
            find_nearest_tsconfig(
                log,
                file_system,
                if stdin.abs_resolve_dir.is_empty() {
                    file_system.cwd()
                } else {
                    &stdin.abs_resolve_dir
                },
                (!options.tsconfig_path.is_empty()).then_some(options.tsconfig_path.as_str()),
            )
        } else {
            None
        };
        if let Some(tsconfig) = &tsconfig {
            tsconfig.jsx_settings.apply_to(&mut file_options.jsx);
            file_options.ts.config = tsconfig.settings;
            file_options.ts_always_strict =
                tsconfig.ts_always_strict_or_strict().cloned().map(Arc::new);
        }
        let loader = if stdin.loader == Loader::None {
            Loader::Js
        } else {
            stdin.loader
        };
        let mut result = parse_file_with_cache(
            log,
            source,
            loader,
            &file_options,
            unique_key_prefix,
            "",
            Some(caches),
        );
        resolve_import_records_from_directory(
            log,
            file_system,
            caches,
            &file_options,
            tsconfig.as_ref(),
            Some(stdin.abs_resolve_dir.as_str()),
            &mut result,
        );
        enqueue_dependencies(&result, caches, &mut queued, &mut pending);
        let needed_length = usize::try_from(source_index).expect("source index fits usize") + 1;
        bundle
            .files
            .resize_with(needed_length, ScannerFile::default);
        if result.ok {
            resolution_slots.insert(source_index, std::mem::take(&mut result.resolve_results));
            bundle.files[usize::try_from(source_index).expect("source index fits usize")] =
                result.file;
            bundle.entry_points.push(GraphEntryPoint {
                output_path: "stdin".into(),
                source_index,
                output_path_was_auto_generated: true,
            });
        }
    }
    let mut automatically_generated_entry_paths = Vec::new();
    let mut expanded_entry_points = Vec::new();
    for entry_point in entry_points {
        if let Some(matches) = expand_entry_point_glob(file_system, &entry_point.input_path) {
            if matches.is_empty() {
                log.add_error(
                    None,
                    Range::default(),
                    format!("Could not resolve {:?}", entry_point.input_path),
                );
            } else {
                expanded_entry_points.extend(matches.into_iter().map(|input_path| EntryPoint {
                    input_path,
                    output_path: entry_point.output_path.clone(),
                    input_path_in_file_namespace: true,
                }));
            }
        } else {
            expanded_entry_points.push(entry_point.clone());
        }
    }
    for entry_point in &expanded_entry_points {
        let input_path_in_file_namespace = entry_point_is_file(file_system, entry_point);
        let input_path = if input_path_in_file_namespace
            && !file_system.is_abs(&entry_point.input_path)
            && !entry_point.input_path.starts_with("./")
            && !entry_point.input_path.starts_with("../")
        {
            format!("./{}", entry_point.input_path)
        } else {
            entry_point.input_path.clone()
        };
        let importer = Path {
            namespace: if input_path_in_file_namespace {
                "file".into()
            } else {
                String::new()
            },
            ..Path::default()
        };
        let (resolved, did_log_plugin_error) = resolve_with_plugins(
            log,
            file_system,
            caches,
            options,
            &importer,
            &input_path,
            &logger::ImportAttributes::default(),
            ImportKind::EntryPoint,
            file_system.cwd(),
            None,
            None,
            Range::default(),
            None,
            false,
        );
        let Some(resolved) = resolved else {
            if !did_log_plugin_error {
                log.add_error(
                    None,
                    Range::default(),
                    format!("Could not resolve {:?}", entry_point.input_path),
                );
            }
            continue;
        };
        if resolved.path_pair.is_external {
            log.add_error(
                None,
                Range::default(),
                format!(
                    "The entry point {:?} cannot be external",
                    entry_point.input_path
                ),
            );
            continue;
        }
        let path = resolved.path_pair.primary.clone();
        let source_index = caches
            .source_index_cache
            .get(path.clone(), SourceIndexKind::Normal);
        let output_path_was_auto_generated = entry_point.output_path.is_empty();
        if output_path_was_auto_generated {
            automatically_generated_entry_paths.push(path.text.clone());
        }
        let output_path = if output_path_was_auto_generated && path.namespace != "file" {
            let mut output_path =
                sanitize_file_path_for_virtual_module_path(&entry_point.input_path);
            let (_, _, extension) = logger::platform_independent_path_dir_base_ext(&output_path);
            output_path.truncate(output_path.len() - extension.len());
            output_path
        } else {
            entry_point.output_path.clone()
        };
        bundle.entry_points.push(GraphEntryPoint {
            output_path,
            source_index,
            output_path_was_auto_generated,
        });
        if queued.insert(source_index) {
            pending.push(PendingFile {
                path,
                source_index,
                resolve_metadata: resolved,
                import_source: None,
                import_path_range: Range::default(),
            });
        }
    }

    if options.abs_output_base.is_empty() {
        options.abs_output_base =
            lowest_common_ancestor_directory(file_system, &automatically_generated_entry_paths);
        if options.abs_output_base.is_empty() {
            options.abs_output_base = file_system.cwd().to_string();
        }
    }

    let mut cursor = 0;
    while cursor < pending.len() {
        let PendingFile {
            path,
            source_index,
            resolve_metadata,
            import_source,
            import_path_range,
        } = pending[cursor].clone();
        cursor += 1;
        let needed_length = usize::try_from(source_index).expect("source index fits usize") + 1;
        if bundle.files.len() < needed_length {
            bundle
                .files
                .resize_with(needed_length, ScannerFile::default);
        }
        let is_disabled = path.is_disabled();
        let mut relative_path = if path.namespace == "file" {
            file_system
                .rel(file_system.cwd(), &path.text)
                .unwrap_or_else(|| path.text.clone())
        } else if path.namespace == "glob" {
            path.text
                .splitn(4, '\0')
                .nth(1)
                .unwrap_or(&path.text)
                .to_string()
        } else if path.namespace == "dataurl" {
            let mut end = path.text.len().min(65);
            while !path.text.is_char_boundary(end) {
                end -= 1;
            }
            let mut pretty = path.text[..end].replace('\n', "\\n");
            if pretty.len() > 64 {
                let mut end = 64;
                while !pretty.is_char_boundary(end) {
                    end -= 1;
                }
                pretty.truncate(end);
                pretty.push_str("...");
            }
            format!("<{pretty}>")
        } else {
            format!("{}:{}", path.namespace, path.text)
        } + &path.ignored_suffix;
        if is_disabled {
            relative_path = format!("(disabled):{relative_path}");
        }
        let absolute_path = if is_disabled {
            relative_path.clone()
        } else if path.namespace == "file" {
            format!("{}{}", path.text, path.ignored_suffix)
        } else if path.namespace == "glob" {
            path.text
                .splitn(4, '\0')
                .next()
                .unwrap_or(&relative_path)
                .to_string()
        } else {
            relative_path.clone()
        };
        let mut source = Source {
            index: source_index,
            key_path: path,
            pretty_paths: logger::PrettyPaths {
                abs: absolute_path,
                rel: relative_path,
            },
            ..Source::default()
        };
        source.identifier_name = if source.key_path.namespace == "glob" {
            String::new()
        } else {
            identifier_name_from_path(file_system, options, &source.key_path.text)
        };
        let Some(loaded) = load_file_with_plugins(
            log,
            file_system,
            caches,
            options,
            &mut source,
            resolve_metadata.plugin_data.clone(),
            import_source.as_ref(),
            import_path_range,
        ) else {
            continue;
        };
        let mut file_options = options.clone();
        let tsconfig = if options.tsconfig_raw.is_empty() && source.key_path.namespace == "file" {
            find_nearest_tsconfig(
                log,
                file_system,
                &file_system.dir(&source.key_path.text),
                (!options.tsconfig_path.is_empty()).then_some(options.tsconfig_path.as_str()),
            )
        } else {
            None
        };
        if let Some(tsconfig) = &tsconfig {
            tsconfig.jsx_settings.apply_to(&mut file_options.jsx);
            file_options.ts.config = tsconfig.settings;
            file_options.ts_always_strict =
                tsconfig.ts_always_strict_or_strict().cloned().map(Arc::new);
        }
        if options.tsconfig_raw.is_empty() {
            resolve_metadata
                .ts_config_jsx
                .apply_to(&mut file_options.jsx);
            if let Some(ts_config) = &resolve_metadata.ts_config {
                file_options.ts.config = *ts_config;
            }
            if let Some(ts_always_strict) = &resolve_metadata.ts_always_strict {
                file_options.ts_always_strict = Some(Arc::new(ts_always_strict.clone()));
            }
        }
        file_options.module_type_data.module_type = if source.key_path.namespace == "file"
            && source.key_path.text.ends_with(".mjs")
        {
            ModuleType::EsmMjs
        } else if source.key_path.namespace == "file" && source.key_path.text.ends_with(".mts") {
            ModuleType::EsmMts
        } else if source.key_path.namespace == "file" && source.key_path.text.ends_with(".cjs") {
            ModuleType::CommonJsCjs
        } else if source.key_path.namespace == "file" && source.key_path.text.ends_with(".cts") {
            ModuleType::CommonJsCts
        } else if source.key_path.namespace == "file"
            && [".js", ".jsx", ".ts", ".tsx"]
                .iter()
                .any(|extension| source.key_path.text.ends_with(extension))
        {
            resolve_metadata.module_type_data.module_type
        } else {
            ModuleType::Unknown
        };
        let is_glob_module = source.key_path.namespace == "glob";
        let mut result = parse_file_with_cache(
            log,
            source,
            loaded.loader,
            &file_options,
            unique_key_prefix,
            &loaded.plugin_name,
            Some(caches),
        );
        if is_glob_module {
            result.file.input_file.omit_from_source_maps_and_metafile = true;
        }
        result.file.plugin_data = loaded.plugin_data;
        if result.file.input_file.side_effects.kind == SideEffectsKind::HasSideEffects
            && let Some(side_effects_data) = &resolve_metadata.primary_side_effects_data
        {
            result.file.input_file.side_effects = SideEffects {
                data: Some(side_effects_data.clone()),
                kind: SideEffectsKind::NoSideEffectsPackageJson,
            };
        }
        resolve_import_records_from_directory(
            log,
            file_system,
            caches,
            &file_options,
            tsconfig.as_ref(),
            Some(&loaded.abs_resolve_dir),
            &mut result,
        );

        enqueue_dependencies(&result, caches, &mut queued, &mut pending);

        if result.ok {
            resolution_slots.insert(source_index, std::mem::take(&mut result.resolve_results));
            bundle.files[usize::try_from(source_index).expect("source index fits usize")] =
                result.file;
        }
    }
    let mut import_attribute_collisions = HashMap::<(String, String, String), usize>::new();
    for file in &bundle.files {
        let path = &file.input_file.source.key_path;
        if path.text.is_empty() {
            continue;
        }
        *import_attribute_collisions
            .entry((
                path.namespace.clone(),
                path.text.clone(),
                path.ignored_suffix.clone(),
            ))
            .or_default() += 1;
    }
    for file in &mut bundle.files {
        let source = &mut file.input_file.source;
        let path = &source.key_path;
        if import_attribute_collisions
            .get(&(
                path.namespace.clone(),
                path.text.clone(),
                path.ignored_suffix.clone(),
            ))
            .copied()
            .unwrap_or_default()
            < 2
        {
            continue;
        }
        let attributes = path.import_attributes.decode_into_array();
        if attributes.is_empty() {
            continue;
        }
        let mut suffix = String::from(" with {");
        for (index, attribute) in attributes.iter().enumerate() {
            if index > 0 {
                suffix.push(',');
            }
            suffix.push(' ');
            if js_ast::is_identifier(&attribute.key) {
                suffix.push_str(&attribute.key);
            } else {
                suffix.push_str(&String::from_utf8_lossy(&quote_single(
                    attribute.key.as_bytes(),
                    false,
                )));
            }
            suffix.push_str(": ");
            suffix.push_str(&String::from_utf8_lossy(&quote_single(
                attribute.value.as_bytes(),
                false,
            )));
        }
        suffix.push_str(" }");
        source.pretty_paths.abs.push_str(&suffix);
        source.pretty_paths.rel.push_str(&suffix);
    }
    warn_about_ignored_bare_imports(log, options, &bundle.files);
    finalize_scan_import_records(log, caches, options, &mut bundle.files, &resolution_slots);
    generate_scan_metadata_chunks(options, &resolution_slots, &mut bundle.files);
    validate_top_level_await(log, options, &mut bundle.files);
    generate_additional_files(
        file_system,
        options,
        &bundle.entry_points,
        &mut bundle.files,
    );
    bundle
}

#[allow(clippy::too_many_lines)]
fn generate_scan_metadata_chunks(
    options: &Options,
    resolution_slots: &HashMap<u32, Vec<Option<ResolveResult>>>,
    files: &mut [ScannerFile],
) {
    if !options.needs_metafile {
        return;
    }
    let fragment = |text: &str| options.metafile_format.maybe_remove_whitespace(text);
    for &source_index in resolution_slots.keys() {
        let index = usize::try_from(source_index).expect("source index fits usize");
        let Some(file) = files.get(index) else {
            continue;
        };
        let source = &file.input_file.source;
        let path = source.pretty_paths.select(options.metafile_path_style);
        let Some(records) = file
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
        else {
            continue;
        };
        let mut metadata = String::from_utf8(quote_for_json(path.as_bytes(), options.ascii_only))
            .expect("quoted JSON is UTF-8");
        metadata.push_str(&fragment(&format!(
            ": {{\n      \"bytes\": {},\n      \"imports\": [",
            source.contents.len()
        )));
        for (record_index, record) in records.iter().enumerate() {
            if record_index != 0 {
                metadata.push(',');
            }
            metadata.push_str(&fragment("\n        {\n          \"path\": "));
            let target_index = if record.source_index.is_valid() {
                Some(record.source_index.get_index())
            } else if record.copy_source_index.is_valid() {
                Some(record.copy_source_index.get_index())
            } else {
                None
            };
            let import_path = target_index
                .and_then(|target| {
                    files.get(usize::try_from(target).expect("source index fits usize"))
                })
                .map_or(record.path.text.as_str(), |target| {
                    target
                        .input_file
                        .source
                        .pretty_paths
                        .select(options.metafile_path_style)
                });
            metadata.push_str(
                &String::from_utf8(quote_for_json(import_path.as_bytes(), options.ascii_only))
                    .expect("quoted JSON is UTF-8"),
            );
            metadata.push_str(&fragment(",\n          \"kind\": "));
            metadata.push_str(
                &String::from_utf8(quote_for_json(
                    record.kind.string_for_metafile().as_bytes(),
                    options.ascii_only,
                ))
                .expect("quoted JSON is UTF-8"),
            );
            if target_index.is_none() {
                metadata.push_str(&fragment(",\n          \"external\": true"));
            } else {
                metadata.push_str(&fragment(",\n          \"original\": "));
                metadata.push_str(
                    &String::from_utf8(quote_for_json(
                        record.path.text.as_bytes(),
                        options.ascii_only,
                    ))
                    .expect("quoted JSON is UTF-8"),
                );
            }
            if let Some(clause) = record
                .assert_or_with
                .as_ref()
                .filter(|clause| clause.keyword == AssertOrWithKeyword::With)
                && !clause.entries.is_empty()
            {
                metadata.push_str(&fragment(",\n          \"with\": {"));
                for (entry_index, entry) in clause.entries.iter().enumerate() {
                    metadata.push_str(&fragment(if entry_index == 0 {
                        "\n            "
                    } else {
                        ",\n            "
                    }));
                    metadata.push_str(
                        &String::from_utf8(quote_for_json(
                            &utf16_to_string(&entry.key),
                            options.ascii_only,
                        ))
                        .expect("quoted JSON is UTF-8"),
                    );
                    metadata.push_str(&fragment(": "));
                    metadata.push_str(
                        &String::from_utf8(quote_for_json(
                            &utf16_to_string(&entry.value),
                            options.ascii_only,
                        ))
                        .expect("quoted JSON is UTF-8"),
                    );
                }
                metadata.push_str(&fragment("\n          }"));
            }
            metadata.push_str(&fragment("\n        }"));
        }
        if !records.is_empty() {
            metadata.push_str(&fragment("\n      "));
        }
        metadata.push(']');
        if let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_ref() {
            let format = match repr.ast.exports_kind {
                ExportsKind::CommonJs => Some("cjs"),
                ExportsKind::Esm | ExportsKind::EsmWithDynamicFallback => Some("esm"),
                ExportsKind::None => None,
            };
            if let Some(format) = format {
                metadata.push_str(&fragment(&format!(",\n      \"format\": {format:?}")));
            }
        }
        metadata.push_str(&fragment("\n    }"));
        files[index].json_metadata_chunk = metadata;
    }
}

#[allow(clippy::too_many_lines)]
fn validate_top_level_await(log: &Log, options: &Options, files: &mut [ScannerFile]) {
    #[allow(clippy::too_many_lines)]
    fn visit(
        source_index: u32,
        log: &Log,
        options: &Options,
        files: &[ScannerFile],
        checks: &mut [TlaCheck],
    ) -> TlaCheck {
        let index = usize::try_from(source_index).expect("source index fits usize");
        if checks[index].depth != 0 {
            return checks[index];
        }
        checks[index].depth = 1;
        let Some(InputFileRepr::Js(repr)) = files[index].input_file.repr.as_ref() else {
            return checks[index];
        };
        if repr.ast.live_top_level_await_keyword.len > 0 {
            checks[index].parent = Index32::new(source_index);
        }
        let records = repr.ast.import_records.clone();
        for (record_index, record) in records.iter().enumerate() {
            if !record.source_index.is_valid()
                || !matches!(record.kind, ImportKind::Stmt | ImportKind::Require)
            {
                continue;
            }
            let parent = visit(record.source_index.get_index(), log, options, files, checks);
            if !parent.parent.is_valid() {
                continue;
            }
            if record.kind == ImportKind::Stmt
                && (!checks[index].parent.is_valid() || parent.depth < checks[index].depth)
            {
                checks[index] = TlaCheck {
                    parent: record.source_index,
                    depth: parent.depth + 1,
                    import_record_index: u32::try_from(record_index)
                        .expect("import record index fits u32"),
                };
                continue;
            }
            if record.kind == ImportKind::Require {
                let mut notes = Vec::new();
                let mut other_source_index = record.source_index.get_index();
                let tla_path = loop {
                    let other_index =
                        usize::try_from(other_source_index).expect("source index fits usize");
                    let other_file = &files[other_index];
                    let Some(InputFileRepr::Js(other_repr)) = &other_file.input_file.repr else {
                        break String::new();
                    };
                    if other_repr.ast.live_top_level_await_keyword.len > 0 {
                        let path = other_file
                            .input_file
                            .source
                            .pretty_paths
                            .select(options.log_path_style)
                            .to_string();
                        let mut tracker =
                            LineColumnTracker::new(Some(&other_file.input_file.source));
                        notes.push(tracker.msg_data(
                            other_repr.ast.live_top_level_await_keyword,
                            format!("The top-level await in {path:?} is here:"),
                        ));
                        break path;
                    }
                    let check = checks[other_index];
                    if !check.parent.is_valid() {
                        break String::new();
                    }
                    let next_source_index = check.parent.get_index();
                    let next_path = files
                        [usize::try_from(next_source_index).expect("source index fits usize")]
                    .input_file
                    .source
                    .pretty_paths
                    .select(options.log_path_style)
                    .to_string();
                    let current_path = other_file
                        .input_file
                        .source
                        .pretty_paths
                        .select(options.log_path_style)
                        .to_string();
                    let mut tracker = LineColumnTracker::new(Some(&other_file.input_file.source));
                    notes.push(tracker.msg_data(
                        other_repr.ast.import_records[check.import_record_index as usize].range,
                        format!("The file {current_path:?} imports the file {next_path:?} here:"),
                    ));
                    other_source_index = next_source_index;
                };
                let imported_path = files[usize::try_from(record.source_index.get_index())
                    .expect("source index fits usize")]
                .input_file
                .source
                .pretty_paths
                .select(options.log_path_style);
                let text = if imported_path == tla_path {
                    format!(
                        "This require call is not allowed because the imported file {imported_path:?} contains a top-level await"
                    )
                } else {
                    format!(
                        "This require call is not allowed because the transitive dependency {tla_path:?} contains a top-level await"
                    )
                };
                let mut tracker = LineColumnTracker::new(Some(&files[index].input_file.source));
                log.add_error_with_notes(Some(&mut tracker), record.range, text, notes);
            }
        }
        checks[index]
    }

    let mut checks = vec![TlaCheck::default(); files.len()];
    for source_index in 0..files.len() {
        visit(
            u32::try_from(source_index).expect("source index fits u32"),
            log,
            options,
            files,
            &mut checks,
        );
    }
    for (file, check) in files.iter_mut().zip(checks) {
        if check.parent.is_valid()
            && let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_mut()
        {
            repr.meta.is_async_or_has_async_dependency = true;
        }
    }
}

#[allow(clippy::too_many_lines)]
fn finalize_scan_import_records(
    log: &Log,
    caches: &CacheSet,
    options: &Options,
    files: &mut Vec<ScannerFile>,
    resolution_slots: &HashMap<u32, Vec<Option<ResolveResult>>>,
) {
    let visited: HashMap<logger::Path, u32> = files
        .iter()
        .filter(|file| !file.input_file.source.key_path.text.is_empty())
        .map(|file| {
            (
                file.input_file.source.key_path.clone(),
                file.input_file.source.index,
            )
        })
        .collect();
    let copy_source_indices: HashSet<u32> = files
        .iter()
        .filter_map(|file| {
            matches!(file.input_file.repr, Some(InputFileRepr::Copy(_)))
                .then_some(file.input_file.source.index)
        })
        .collect();

    for (source_index, slots) in resolution_slots {
        let Some(records) = files
            .get_mut(usize::try_from(*source_index).expect("source index fits usize"))
            .and_then(|file| file.input_file.repr.as_mut())
            .and_then(InputFileRepr::import_records_mut)
        else {
            continue;
        };
        for (record, resolve_result) in records.iter_mut().zip(slots) {
            if let Some(resolve_result) = resolve_result
                && resolve_result.path_pair.has_secondary()
                && let Some(secondary_source_index) =
                    visited.get(&resolve_result.path_pair.secondary)
            {
                record.source_index = Index32::new(*secondary_source_index);
            }
            if record.source_index.is_valid()
                && copy_source_indices.contains(&record.source_index.get_index())
            {
                record.copy_source_index = record.source_index;
                record.source_index = Index32::default();
            }
        }
    }

    validate_scan_imports(log, options, files);

    let mut css_edges = Vec::new();
    for (importer_index, file) in files.iter().enumerate() {
        let Some(InputFileRepr::Js(repr)) = &file.input_file.repr else {
            continue;
        };
        for (record_index, record) in repr.ast.import_records.iter().enumerate() {
            if !record.source_index.is_valid() {
                continue;
            }
            let target_index = record.source_index.get_index();
            if files
                .get(usize::try_from(target_index).expect("source index fits usize"))
                .is_some_and(|target| matches!(target.input_file.repr, Some(InputFileRepr::Css(_))))
            {
                css_edges.push((
                    u32::try_from(importer_index).expect("source index fits u32"),
                    u32::try_from(record_index).expect("import record index fits u32"),
                    target_index,
                ));
            }
        }
    }

    let mut css_stubs = HashMap::new();
    for (importer_index, record_index, css_source_index) in css_edges {
        if options.write_to_stdout {
            let target_path = files
                [usize::try_from(css_source_index).expect("source index fits usize")]
            .input_file
            .source
            .pretty_paths
            .select(options.log_path_style)
            .to_string();
            log.add_error(
                None,
                Range::default(),
                format!(
                    "Cannot import {target_path:?} into a JavaScript file without an output path configured"
                ),
            );
            let Some(InputFileRepr::Js(importer_repr)) = files
                [usize::try_from(importer_index).expect("source index fits usize")]
            .input_file
            .repr
            .as_mut() else {
                continue;
            };
            importer_repr.ast.import_records
                [usize::try_from(record_index).expect("import record index fits usize")]
            .source_index = Index32::default();
            continue;
        }

        let stub_source_index = *css_stubs.entry(css_source_index).or_insert_with(|| {
            let css_file =
                &files[usize::try_from(css_source_index).expect("source index fits usize")];
            caches.source_index_cache.get(
                css_file.input_file.source.key_path.clone(),
                SourceIndexKind::JsStubForCss,
            )
        });
        let stub_index = usize::try_from(stub_source_index).expect("source index fits usize");
        if files.len() <= stub_index {
            files.resize_with(stub_index + 1, ScannerFile::default);
        }
        if files[stub_index].input_file.repr.is_none() {
            let css_file =
                &files[usize::try_from(css_source_index).expect("source index fits usize")];
            let mut source = css_file.input_file.source.clone();
            source.index = stub_source_index;
            let loader = css_file.input_file.loader;
            let ast = js_parser::lazy_export_ast(
                log.clone(),
                &source,
                js_parser::options_from_config(options),
                Expr::new(logger::Loc::default(), ExprData::Null),
                None,
            );
            files[stub_index] = ScannerFile {
                input_file: InputFile {
                    source,
                    loader,
                    repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                        ast,
                        css_source_index: Index32::new(css_source_index),
                        ..JsRepr::default()
                    }))),
                    ..InputFile::default()
                },
                ..ScannerFile::default()
            };
        }
        if let Some(InputFileRepr::Css(css_repr)) = files
            [usize::try_from(css_source_index).expect("source index fits usize")]
        .input_file
        .repr
        .as_mut()
        {
            css_repr.js_source_index = Index32::new(stub_source_index);
        }
        if let Some(InputFileRepr::Js(importer_repr)) = files
            [usize::try_from(importer_index).expect("source index fits usize")]
        .input_file
        .repr
        .as_mut()
        {
            importer_repr.ast.import_records
                [usize::try_from(record_index).expect("import record index fits usize")]
            .source_index = Index32::new(stub_source_index);
        }
    }
}

fn validate_scan_imports(log: &Log, options: &Options, files: &[ScannerFile]) {
    for file in files {
        let Some(records) = file
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
        else {
            continue;
        };
        for record in records {
            if !record.source_index.is_valid() {
                continue;
            }
            let Some(target) = files
                .get(usize::try_from(record.source_index.get_index()).expect("source index fits"))
            else {
                continue;
            };
            let target_path = target
                .input_file
                .source
                .pretty_paths
                .select(options.log_path_style);
            if record.flags.contains(ImportRecordFlags::ASSERT_TYPE_JSON)
                && !matches!(target.input_file.loader, Loader::Json | Loader::Copy)
            {
                let loader_name =
                    crate::internal::config::LOADER_TO_STRING[target.input_file.loader as usize];
                log.add_error(
                    None,
                    record.range,
                    format!("The file {target_path:?} was loaded with the {loader_name:?} loader"),
                );
            }
            match record.kind {
                ImportKind::ComposesFrom
                    if matches!(target.input_file.repr, Some(InputFileRepr::Js(_)))
                        && target.input_file.loader != Loader::Empty =>
                {
                    log.add_error(
                        None,
                        record.range,
                        format!("Cannot use \"composes\" with {target_path:?}"),
                    );
                }
                ImportKind::At
                    if matches!(target.input_file.repr, Some(InputFileRepr::Js(_)))
                        && target.input_file.loader != Loader::Empty =>
                {
                    log.add_error(
                        None,
                        record.range,
                        format!("Cannot import {target_path:?} into a CSS file"),
                    );
                }
                ImportKind::Url => match &target.input_file.repr {
                    Some(InputFileRepr::Css(_)) => {
                        log.add_error(
                            None,
                            record.range,
                            format!("Cannot use {target_path:?} as a URL"),
                        );
                    }
                    Some(InputFileRepr::Js(repr))
                        if repr.ast.url_for_css.is_empty()
                            && target.input_file.loader != Loader::Empty =>
                    {
                        log.add_error(
                            None,
                            record.range,
                            format!("Cannot use {target_path:?} as a URL"),
                        );
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn generate_additional_files(
    file_system: &dyn Fs,
    options: &Options,
    entry_points: &[GraphEntryPoint],
    files: &mut [ScannerFile],
) {
    for file in files {
        if file.input_file.unique_key_for_additional_file.is_empty() {
            continue;
        }
        let bytes = file.input_file.source.contents.to_vec();
        let entry_point = entry_points
            .iter()
            .find(|entry_point| entry_point.source_index == file.input_file.source.index);
        let is_copy_entry = file.input_file.loader == Loader::Copy && entry_point.is_some();
        let template = if is_copy_entry {
            &options.entry_path_template
        } else {
            &options.asset_path_template
        };
        let custom_file_path = if is_copy_entry {
            entry_point
                .map(|entry_point| entry_point.output_path.as_str())
                .unwrap_or_default()
        } else {
            ""
        };
        let use_output_file = is_copy_entry && !options.abs_output_file.is_empty();

        let hash = if has_placeholder(template, PathPlaceholder::Hash) {
            let mut digest = xxhash::Digest::new();
            digest.write(&bytes);
            hash_for_file_name(&digest.sum(&[]))
        } else {
            String::new()
        };
        let (directory, base, extension) = if use_output_file {
            let base_with_extension = file_system.base(&options.abs_output_file);
            let extension = file_system.ext(&base_with_extension);
            let base = base_with_extension[..base_with_extension.len() - extension.len()].into();
            ("/".into(), base, extension)
        } else {
            let (_, _, extension) = logger::platform_independent_path_dir_base_ext(
                &file.input_file.source.key_path.text,
            );
            let (directory, base) = path_relative_to_outbase(
                &file.input_file,
                options,
                file_system,
                false,
                custom_file_path,
            );
            (directory, base, extension)
        };
        let extension_without_dot = extension.strip_prefix('.').unwrap_or(&extension);
        let relative_path = format!(
            "{}{extension}",
            template_to_string(&substitute_template(
                template,
                &PathPlaceholders {
                    dir: Some(directory),
                    name: Some(base),
                    hash: Some(hash),
                    ext: Some(extension_without_dot.into()),
                },
            ))
        );
        let json_metadata_chunk = if options.needs_metafile {
            let input_path = String::from_utf8(quote_for_json(
                file.input_file
                    .source
                    .pretty_paths
                    .select(options.metafile_path_style)
                    .as_bytes(),
                options.ascii_only,
            ))
            .expect("quoted JSON is UTF-8");
            let entry_point_json = if is_copy_entry {
                format!("\"entryPoint\": {input_path},\n      ")
            } else {
                String::new()
            };
            options.metafile_format.maybe_remove_whitespace(&format!(
                "{{\n      \"imports\": [],\n      \"exports\": [],\n      {entry_point_json}\"inputs\": {{\n        {input_path}: {{\n          \"bytesInOutput\": {}\n        }}\n      }},\n      \"bytes\": {}\n    }}",
                bytes.len(),
                bytes.len()
            ))
        } else {
            String::new()
        };
        file.input_file.additional_files = vec![OutputFile {
            abs_path: file_system.join(&[&options.abs_output_dir, &relative_path]),
            contents: bytes,
            json_metadata_chunk,
            ..OutputFile::default()
        }];
    }
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
    } else if !contents
        .iter()
        .any(|byte| *byte < 0x20 && !matches!(*byte, b'\t' | b'\n' | b'\r' | b'\x0c'))
    {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

#[allow(clippy::too_many_lines)]
pub fn apply_option_defaults(options: &mut Options) {
    if options.extension_order.is_empty() {
        options.extension_order = [".tsx", ".ts", ".jsx", ".js", ".css", ".json"]
            .into_iter()
            .map(str::to_string)
            .collect();
    }
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

    apply_unsupported_feature_constraints(options);
}

pub(crate) fn apply_unsupported_feature_constraints(options: &mut Options) {
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
        apply_option_defaults, bundle_javascript, default_extension_to_loader_map,
        find_reachable_files, guess_mime_type, hash_for_file_name, is_ascii_only, parse_file,
        parse_file_with_unique_key_prefix, path_relative_to_outbase, resolve_import_records,
        sanitize_file_path_for_virtual_module_path, scan_bundle,
    };
    use crate::internal::{
        ast::{ImportKind, ImportRecord, ImportRecordFlags, Index32},
        cache::CacheSet,
        compat::JsFeature,
        config::{
            self, Format, Loader, Mode, OnLoad, OnLoadResult, OnResolve, OnResolveResult, Options,
            PathPlaceholder, Platform, Plugin, compile_filter_for_plugin,
        },
        fs::{MockKind, mock_fs, mock_fs_bytes},
        graph::{EntryPoint, InputFile, InputFileRepr, JsRepr, SideEffectsKind},
        js_ast::ExportsKind,
        logger::{DeferLogKind, Log, Msg, MsgKind, Path, Source},
        runtime,
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    fn upstream_numeric_option(options: &serde_json::Value, name: &str) -> Option<u64> {
        options.get(name).and_then(serde_json::Value::as_u64)
    }

    fn upstream_u64_option(options: &serde_json::Value, name: &str) -> Option<u64> {
        options.get(name).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    }

    fn upstream_string_option(options: &serde_json::Value, name: &str) -> Option<String> {
        options
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    fn upstream_bool_option(options: &serde_json::Value, name: &str) -> Option<bool> {
        options.get(name).and_then(serde_json::Value::as_bool)
    }

    fn upstream_mode(value: u64) -> Mode {
        match value {
            0 => Mode::PassThrough,
            1 => Mode::ConvertFormat,
            2 => Mode::Bundle,
            _ => panic!("unknown upstream mode {value}"),
        }
    }

    fn upstream_format(value: u64) -> Format {
        match value {
            0 => Format::Preserve,
            1 => Format::Iife,
            2 => Format::CommonJs,
            3 => Format::EsModule,
            _ => panic!("unknown upstream format {value}"),
        }
    }

    fn upstream_platform(value: u64) -> Platform {
        match value {
            0 => Platform::Browser,
            1 => Platform::Node,
            2 => Platform::Neutral,
            _ => panic!("unknown upstream platform {value}"),
        }
    }

    fn upstream_legal_comments(value: u64) -> config::LegalComments {
        match value {
            0 => config::LegalComments::Inline,
            1 => config::LegalComments::None,
            2 => config::LegalComments::EndOfFile,
            3 => config::LegalComments::LinkedWithComment,
            4 => config::LegalComments::ExternalWithoutComment,
            _ => panic!("unknown upstream legal-comments mode {value}"),
        }
    }

    fn upstream_loader(value: u64) -> Loader {
        match value {
            0 => Loader::None,
            1 => Loader::Base64,
            2 => Loader::Binary,
            3 => Loader::Copy,
            4 => Loader::Css,
            5 => Loader::DataUrl,
            6 => Loader::Default,
            7 => Loader::Empty,
            8 => Loader::File,
            9 => Loader::GlobalCss,
            10 => Loader::Js,
            11 => Loader::Json,
            12 => Loader::WithTypeJson,
            13 => Loader::Jsx,
            14 => Loader::LocalCss,
            15 => Loader::Text,
            16 => Loader::Ts,
            17 => Loader::TsNoAmbiguousLessThan,
            18 => Loader::Tsx,
            _ => panic!("unknown upstream loader {value}"),
        }
    }

    #[test]
    fn matches_upstream_missing_glob_directory_diagnostics() {
        let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([(
                "/entry.js".into(),
                "const x = 'a.js'; require('./src/' + x); import('./src/' + x)".into(),
            )]),
            MockKind::Unix,
            "/",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            code_splitting: true,
            abs_output_dir: "/out".into(),
            output_format: Format::EsModule,
            omit_runtime_for_tests: true,
            tree_shaking: true,
            ..Options::default()
        };
        let _ = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "/entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "UPSTREAM_TEST",
        );
        let diagnostics: Vec<u8> = log
            .done()
            .iter()
            .flat_map(|message| {
                message.to_bytes(
                    &crate::internal::logger::OutputOptions::default(),
                    crate::internal::logger::TerminalInfo::default(),
                )
            })
            .collect();
        assert_eq!(
            String::from_utf8(diagnostics).expect("diagnostics are UTF-8"),
            "entry.js: ERROR: Could not resolve require(\"./src/**/*\")\n\
             entry.js: ERROR: Could not resolve import(\"./src/**/*\")\n"
        );
    }

    #[test]
    fn matches_pinned_upstream_active_bundler_corpus() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/upstream/bundler.json"
        )))
        .expect("valid generated upstream bundler corpus");
        let cases = cases.as_array().expect("bundler corpus array");
        let selected_test = std::env::var("ESBUILD_RS_UPSTREAM_TEST").ok();
        let mut matched = 0;

        for case in cases {
            if selected_test
                .as_deref()
                .is_some_and(|selected| case["upstream_test"] != selected)
            {
                continue;
            }
            let options_json = &case["options"];
            let Some(option_names) = options_json.as_object().map(|options| {
                let mut names: Vec<_> = options.keys().map(String::as_str).collect();
                names.sort_unstable();
                names
            }) else {
                panic!("bundler case options must be an object");
            };
            if case["file_system"] != "unix"
                || (case["suite"] != "default"
                    && case["suite"] != "importstar"
                    && case["suite"] != "importstar_ts"
                    && case["suite"] != "lower"
                    && case["suite"] != "dce"
                    && case["suite"] != "ts"
                    && case["suite"] != "packagejson"
                    && case["suite"] != "tsconfig"
                    && case["suite"] != "loader"
                    && case["suite"] != "css"
                    && case["suite"] != "splitting"
                    && case["suite"] != "yarnpnp"
                    && case["suite"] != "glob")
                || ((!case["expected_scan_log"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty()
                    || !case["expected_compile_log"]
                        .as_str()
                        .unwrap_or_default()
                        .is_empty())
                    && case["upstream_test"] != "TestGlobNoMatches"
                    && case["upstream_test"] != "TestCSSMalformedAtImport"
                    && case["upstream_test"] != "TestTsconfigJsonExtendsLoop"
                    && case["upstream_test"] != "TestTsconfigJsonNodeModulesTsconfigPathBad"
                    && case["upstream_test"] != "TestTsconfigWarningsInsideNodeModules"
                    && case["upstream_test"] != "TestTsconfigUnrecognizedTargetWarning"
                    && case["upstream_test"] != "TestTsconfigJsonTopLevelMistakeWarning"
                    && case["upstream_test"]
                        != "TestPackageJsonSideEffectsFalseKeepBareImportAndRequireES6"
                    && case["upstream_test"]
                        != "TestPackageJsonSideEffectsFalseKeepBareImportAndRequireCommonJS"
                    && case["upstream_test"]
                        != "TestPackageJsonSideEffectsFalseRemoveBareImportES6"
                    && case["upstream_test"]
                        != "TestPackageJsonSideEffectsFalseRemoveBareImportCommonJS"
                    && case["upstream_test"] != "TestPackageJsonSideEffectsArrayGlob"
                    && case["upstream_test"] != "TestTSSideEffectsFalseWarningTypeDeclarations"
                    && case["upstream_test"] != "TestDeadCodeInsideUnusedCases"
                    && case["upstream_test"] != "TestDuplicatePropertyWarning"
                    && case["upstream_test"] != "TestLowerExponentiationOperatorNoBundle"
                    && case["upstream_test"] != "TestLowerConstIssue4448"
                    && case["upstream_test"] != "TestImportMetaCommonJS")
                || case.get("expected_snapshot").is_none()
                || !case["entry_paths"].is_array()
                || case["entry_paths"]
                    .as_array()
                    .is_some_and(|paths| paths.iter().any(|path| path == "*"))
                || case
                    .get("unsupported_options")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|options| !options.is_empty())
                || (option_names != ["AbsOutputFile", "Mode"]
                    && option_names != ["AbsOutputFile", "Mode", "OutputFormat"]
                    && option_names != ["AbsOutputDir", "Mode"]
                    && !((option_names == ["AbsOutputFile", "ExtensionToLoader", "Mode"]
                        || option_names == ["AbsOutputDir", "ExtensionToLoader", "Mode"])
                        && !matches!(
                            case["upstream_test"].as_str(),
                            Some(
                                "TestImportCSSFromJSLocalVsGlobal" | "TestImportCSSFromJSComposes"
                            )
                        ))
                    && option_names != ["AbsOutputFile"]
                    && option_names != ["AbsOutputDir"]
                    && option_names != ["AbsOutputFile", "Mode", "Platform"]
                    && option_names != ["AbsOutputDir", "Mode", "Platform"]
                    && option_names != ["AbsOutputFile", "Mode", "OutputFormat", "Platform"]
                    && option_names != ["AbsOutputDir", "Mode", "OutputFormat", "Platform"]
                    && option_names != ["AbsOutputDir", "LegalComments", "Mode"]
                    && option_names != ["AbsOutputFile", "LegalComments", "Mode"]
                    && option_names
                        != ["AbsOutputDir", "LegalComments", "MinifyWhitespace", "Mode"]
                    && option_names != ["AbsOutputDir", "KeepNames"]
                    && option_names != ["AbsOutputFile", "KeepNames"]
                    && option_names != ["AbsOutputDir", "KeepNames", "MinifySyntax"]
                    && option_names != ["AbsOutputFile", "KeepNames", "MinifySyntax"]
                    && option_names != ["AbsOutputFile", "KeepNames", "MinifySyntax", "Mode"]
                    && option_names != ["AbsOutputDir", "MinifySyntax"]
                    && option_names != ["AbsOutputFile", "MinifySyntax"]
                    && option_names != ["AbsOutputFile", "MinifySyntax", "MinifyWhitespace"]
                    && option_names != ["AbsOutputDir", "MinifyWhitespace"]
                    && option_names != ["AbsOutputDir", "MinifySyntax", "Mode"]
                    && option_names != ["AbsOutputFile", "MinifySyntax", "Mode"]
                    && option_names != ["AbsOutputFile", "MinifySyntax", "Mode", "OutputFormat"]
                    && option_names
                        != ["AbsOutputFile", "MinifyWhitespace", "Mode", "OutputFormat"]
                    && option_names
                        != [
                            "AbsOutputFile",
                            "MinifyWhitespace",
                            "Mode",
                            "OutputFormat",
                            "Platform",
                        ]
                    && option_names != ["AbsOutputFile", "KeepNames", "Mode"]
                    && !(option_names == ["AbsOutputFile", "Mode", "TS"]
                        && matches!(
                            case["upstream_test"].as_str(),
                            Some(
                                "TestThisInsideFunctionTS"
                                    | "TestThisInsideFunctionTSUseDefineForClassFields"
                                    | "TestParameterPropsUseDefineForClassFieldsTrue"
                                    | "TestParameterPropsUseDefineForClassFieldsFalse"
                            )
                        ))
                    && !(option_names == ["AbsOutputFile", "TS"]
                        && matches!(
                            case["upstream_test"].as_str(),
                            Some(
                                "TestThisInsideFunctionTSNoBundleUseDefineForClassFields"
                                    | "TestTSAbstractClassFieldUseAssign"
                                    | "TestTSAbstractClassFieldUseDefine"
                            )
                        ))
                    && !(option_names == ["AbsOutputFile", "Mode", "TS", "UnsupportedJSFeatures"]
                        && case["upstream_test"]
                            == "TestParameterPropsUseDefineForClassFieldsTrueLowered")
                    && !(option_names
                        == [
                            "AbsOutputFile",
                            "KeepNames",
                            "Mode",
                            "UnsupportedJSFeatures",
                        ]
                        && case["upstream_test"] == "TestTSNamespaceKeepNamesTargetES2015")
                    && !(option_names == ["AbsOutputFile", "Mode", "UnsupportedJSFeatures"]
                        && matches!(
                            case["upstream_test"].as_str(),
                            Some("TestTSDeclareClassFields" | "TestStaticClassBlockES2021")
                        ))
                    && !(option_names
                        == ["AbsOutputFile", "MinifySyntax", "UnsupportedJSFeatures"]
                        && case["upstream_test"] == "TestTSMinifyDerivedClass")
                    && !(option_names == ["AbsOutputFile", "UnsupportedJSFeatures"]
                        && matches!(
                            case["upstream_test"].as_str(),
                            Some(
                                "TestLowerOptionalCatchNameCollisionNoBundle"
                                    | "TestLowerObjectSpreadNoBundle"
                                    | "TestLowerExponentiationOperatorNoBundle"
                                    | "TestLowerAsync2017NoBundle"
                                    | "TestLowerTemplateObject"
                                    | "TestLowerConstIssue4448"
                                    | "TestTSLowerObjectRest2018NoBundle"
                                    | "TestTSComputedClassFieldUseDefineTrueLower"
                            )
                        ))
                    && option_names != ["AbsOutputDir", "MinifyIdentifiers"]
                    && option_names != ["AbsOutputFile", "MinifyIdentifiers"]
                    && option_names != ["AbsOutputDir", "MinifyIdentifiers", "Mode"]
                    && option_names != ["AbsOutputFile", "MinifyIdentifiers", "Mode"]
                    && option_names
                        != [
                            "AbsOutputDir",
                            "MinifyIdentifiers",
                            "MinifySyntax",
                            "MinifyWhitespace",
                        ]
                    && option_names
                        != [
                            "AbsOutputFile",
                            "MinifyIdentifiers",
                            "MinifySyntax",
                            "MinifyWhitespace",
                            "Mode",
                        ]
                    && option_names != ["AbsOutputFile", "MinifyIdentifiers", "MinifyWhitespace"]
                    && option_names
                        != [
                            "AbsOutputDir",
                            "MinifyIdentifiers",
                            "MinifyWhitespace",
                            "Mode",
                        ]
                    && option_names
                        != ["AbsOutputFile", "MinifyIdentifiers", "Mode", "OutputFormat"]
                    && option_names
                        != [
                            "AbsOutputFile",
                            "MinifyIdentifiers",
                            "Mode",
                            "OutputFormat",
                            "Platform",
                        ]
                    && option_names != ["AbsOutputFile", "TreeShaking"]
                    && option_names != ["AbsOutputDir", "MinifySyntax", "TreeShaking"]
                    && option_names != ["AbsOutputDir", "Mode", "TreeShaking"]
                    && option_names != ["AbsOutputDir", "CodeSplitting", "Mode"]
                    && option_names != ["AbsOutputDir", "CodeSplitting", "Mode", "OutputFormat"]
                    && option_names
                        != [
                            "AbsOutputDir",
                            "CodeSplitting",
                            "MinifyWhitespace",
                            "Mode",
                            "OutputFormat",
                        ]
                    && option_names
                        != [
                            "AbsOutputDir",
                            "CodeSplitting",
                            "MinifyIdentifiers",
                            "Mode",
                            "OutputFormat",
                        ]
                    && option_names
                        != [
                            "AbsOutputDir",
                            "CodeSplitting",
                            "Mode",
                            "OutputFormat",
                            "PublicPath",
                        ]
                    && option_names
                        != [
                            "AbsOutputDir",
                            "ChunkPathTemplate",
                            "CodeSplitting",
                            "Mode",
                            "OutputFormat",
                        ])
            {
                continue;
            }
            matched += 1;

            let test_name = case["upstream_test"]
                .as_str()
                .expect("upstream bundler test name");
            let mut files: HashMap<String, Vec<u8>> = case["files"]
                .as_object()
                .expect("upstream bundler files")
                .iter()
                .map(|(path, contents)| {
                    (
                        path.clone(),
                        contents
                            .as_str()
                            .expect("upstream bundler file contents")
                            .as_bytes()
                            .to_vec(),
                    )
                })
                .collect();
            if let Some(encoded_files) = case
                .get("files_base64")
                .and_then(serde_json::Value::as_object)
            {
                for (path, contents) in encoded_files {
                    let contents = contents.as_str().expect("upstream base64 file contents");
                    files.insert(
                        path.clone(),
                        STANDARD
                            .decode(contents)
                            .expect("valid upstream base64 file contents"),
                    );
                }
            }
            let abs_working_dir = case["abs_working_dir"]
                .as_str()
                .filter(|path| !path.is_empty())
                .unwrap_or("/");
            let file_system = mock_fs_bytes(&files, MockKind::Unix, abs_working_dir);
            let abs_output_file =
                upstream_string_option(options_json, "AbsOutputFile").unwrap_or_default();
            let abs_output_dir = upstream_string_option(options_json, "AbsOutputDir")
                .unwrap_or_else(|| {
                    abs_output_file.rsplit_once('/').map_or_else(
                        || "/".to_string(),
                        |(directory, _)| {
                            if directory.is_empty() {
                                "/".to_string()
                            } else {
                                directory.to_string()
                            }
                        },
                    )
                });
            let mut options = Options {
                mode: upstream_numeric_option(options_json, "Mode")
                    .map_or(Mode::PassThrough, upstream_mode),
                abs_output_file,
                abs_output_dir,
                omit_runtime_for_tests: true,
                ..Options::default()
            };
            if let Some(format) = upstream_numeric_option(options_json, "OutputFormat") {
                options.output_format = upstream_format(format);
            }
            if options.mode == Mode::Bundle {
                options.tree_shaking = true;
                if options.output_format == Format::Preserve {
                    options.output_format = Format::EsModule;
                }
            } else if options.mode == Mode::ConvertFormat && options.output_format == Format::Iife {
                options.tree_shaking = true;
            }
            if let Some(platform) = upstream_numeric_option(options_json, "Platform") {
                options.platform = upstream_platform(platform);
            }
            if let Some(legal_comments) = upstream_numeric_option(options_json, "LegalComments") {
                options.legal_comments = upstream_legal_comments(legal_comments);
            }
            if let Some(minify_whitespace) = upstream_bool_option(options_json, "MinifyWhitespace")
            {
                options.minify_whitespace = minify_whitespace;
            }
            if let Some(keep_names) = upstream_bool_option(options_json, "KeepNames") {
                options.keep_names = keep_names;
            }
            if let Some(minify_syntax) = upstream_bool_option(options_json, "MinifySyntax") {
                options.minify_syntax = minify_syntax;
            }
            if let Some(tree_shaking) = upstream_bool_option(options_json, "TreeShaking") {
                options.tree_shaking = tree_shaking;
            }
            if let Some(code_splitting) = upstream_bool_option(options_json, "CodeSplitting") {
                options.code_splitting = code_splitting;
            }
            if let Some(minify_identifiers) =
                upstream_bool_option(options_json, "MinifyIdentifiers")
            {
                options.minify_identifiers = minify_identifiers;
            }
            if let Some(features) = upstream_u64_option(options_json, "UnsupportedJSFeatures") {
                options.unsupported_js_features =
                    crate::internal::compat::JsFeature::from_bits(features);
            }
            if let Some(loaders) = options_json
                .get("ExtensionToLoader")
                .and_then(serde_json::Value::as_object)
            {
                options.extension_to_loader = loaders
                    .iter()
                    .map(|(extension, loader)| {
                        let loader = loader.as_u64().expect("upstream loader enum");
                        (extension.clone(), upstream_loader(loader))
                    })
                    .collect();
            }
            if let Some(value) = options_json
                .pointer("/TS/Config/UseDefineForClassFields")
                .and_then(serde_json::Value::as_u64)
            {
                options.ts.config.use_define_for_class_fields = match value {
                    1 => crate::internal::config::MaybeBool::True,
                    2 => crate::internal::config::MaybeBool::False,
                    _ => crate::internal::config::MaybeBool::Unspecified,
                };
            }
            if let Some(public_path) = upstream_string_option(options_json, "PublicPath") {
                options.public_path = public_path;
            }
            if let Some(template) = options_json
                .get("ChunkPathTemplate")
                .and_then(serde_json::Value::as_array)
            {
                options.chunk_path_template = template
                    .iter()
                    .map(|part| config::PathTemplate {
                        data: part["Data"].as_str().unwrap_or_default().to_string(),
                        placeholder: match part["Placeholder"].as_u64().unwrap_or_default() {
                            1 => config::PathPlaceholder::Dir,
                            2 => config::PathPlaceholder::Name,
                            3 => config::PathPlaceholder::Hash,
                            4 => config::PathPlaceholder::Ext,
                            _ => config::PathPlaceholder::None,
                        },
                    })
                    .collect();
            }
            let entry_points: Vec<super::EntryPoint> = case["entry_paths"]
                .as_array()
                .expect("upstream bundler entry paths")
                .iter()
                .map(|path| super::EntryPoint {
                    input_path: path.as_str().expect("upstream entry path").to_string(),
                    ..super::EntryPoint::default()
                })
                .collect();
            let log = Log::new_defer(
                if case["debug_logs"].as_bool().unwrap_or_default() {
                    DeferLogKind::All
                } else {
                    DeferLogKind::NoVerboseOrDebug
                },
                HashMap::new(),
            );
            let compiled = bundle_javascript(
                &log,
                &file_system,
                &CacheSet::default(),
                &entry_points,
                &mut options,
                "UPSTREAM_TEST",
            );
            let messages = log.done();
            let diagnostic_texts: Vec<_> = messages
                .iter()
                .map(|message| {
                    message.data.location.as_ref().map_or_else(
                        || message.data.text.clone(),
                        |location| {
                            format!(
                                "{}:{}:{}: {}",
                                location.file.abs,
                                location.line,
                                location.column,
                                message.data.text
                            )
                        },
                    )
                })
                .collect();
            let diagnostic_bytes: Vec<u8> = messages
                .iter()
                .flat_map(|message| {
                    message.to_bytes(
                        &crate::internal::logger::OutputOptions::default(),
                        crate::internal::logger::TerminalInfo::default(),
                    )
                })
                .collect();
            let diagnostics =
                String::from_utf8(diagnostic_bytes).expect("upstream diagnostics are valid UTF-8");
            let expected_diagnostics = format!(
                "{}{}",
                case["expected_scan_log"].as_str().unwrap_or_default(),
                case["expected_compile_log"].as_str().unwrap_or_default()
            );
            assert_eq!(
                diagnostics, expected_diagnostics,
                "{test_name}: diagnostic mismatch; messages: {diagnostic_texts:#?}"
            );

            let mut generated = String::new();
            for output in compiled.output_files {
                if !generated.is_empty() {
                    generated.push('\n');
                }
                generated.push_str(&format!(
                    "---------- {} ----------\n{}",
                    output.abs_path,
                    String::from_utf8_lossy(&output.contents)
                ));
            }
            if !compiled.metafile.is_empty() {
                generated.push_str(&format!(
                    "---------- metafile.json ----------\n{}",
                    compiled.metafile
                ));
            }
            assert_eq!(
                generated,
                case["expected_snapshot"]
                    .as_str()
                    .expect("upstream expected bundler snapshot"),
                "upstream bundler test {test_name}"
            );
        }

        assert_eq!(
            matched,
            if selected_test.is_some() { 1 } else { 626 },
            "upstream basic bundler corpus case count"
        );
    }

    #[test]
    fn applies_upstream_option_defaults() {
        let mut options = Options {
            platform: Platform::Node,
            ..Options::default()
        };
        apply_option_defaults(&mut options);
        assert_eq!(
            options.extension_order,
            [".tsx", ".ts", ".jsx", ".js", ".css", ".json"]
        );
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
        assert_eq!(guess_mime_type("", &[0xff]), "text/plain;charset=utf-8");
        assert_eq!(
            guess_mime_type("", &[0xff, 0x00]),
            "application/octet-stream"
        );
    }

    #[test]
    fn resolves_parsed_import_records_into_source_indexes() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                ("/project/entry.js".into(), String::new()),
                ("/project/dep.js".into(), "export let x = 1".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let options = Options {
            extension_order: vec![".js".into()],
            mode: Mode::Bundle,
            ..Options::default()
        };
        let mut result = parse_file(
            &log,
            source(
                "/project/entry.js",
                b"import './dep'; import {x} from './dep'; console.log(x)",
            ),
            Loader::Js,
            &options,
        );
        let cache = CacheSet::default();
        resolve_import_records(&log, &file_system, &cache, &options, None, &mut result);

        assert_eq!(result.resolve_results.len(), 2);
        assert!(result.resolve_results.iter().all(Option::is_some));
        let records = result
            .file
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("JavaScript import records");
        assert!(records.iter().all(|record| record.source_index.is_valid()));
        assert_eq!(
            records[0].source_index.get_index(),
            records[1].source_index.get_index()
        );
        assert_eq!(
            result.resolve_results[0]
                .as_ref()
                .expect("resolution")
                .path_pair
                .primary
                .text,
            "/project/dep.js"
        );
        assert!(log.done().is_empty());
    }

    #[test]
    fn externalizes_node_builtins_and_suppresses_handled_failures() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let options = Options {
            extension_order: vec![".js".into()],
            mode: Mode::Bundle,
            platform: Platform::Node,
            ..Options::default()
        };
        let mut result = parse_file(
            &log,
            source(
                "/project/entry.js",
                b"import fs from 'node:fs'; import('./missing').catch(() => {})",
            ),
            Loader::Js,
            &options,
        );
        {
            let records = result
                .file
                .input_file
                .repr
                .as_mut()
                .and_then(InputFileRepr::import_records_mut)
                .expect("JavaScript import records");
            assert_eq!(records[0].kind, ImportKind::Stmt);
            records[1].flags |= ImportRecordFlags::HANDLES_IMPORT_ERRORS;
        }
        let cache = CacheSet::default();
        resolve_import_records(&log, &file_system, &cache, &options, None, &mut result);

        let records = result
            .file
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("JavaScript import records");
        assert!(!records[0].source_index.is_valid());
        assert!(
            result.resolve_results[0]
                .as_ref()
                .expect("node builtin resolution")
                .path_pair
                .is_external
        );
        assert!(result.resolve_results[1].is_none());
        assert!(log.done().is_empty());
    }

    #[test]
    fn logs_unresolved_imports_during_dependency_scanning() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let options = Options {
            extension_order: vec![".js".into()],
            mode: Mode::Bundle,
            ..Options::default()
        };
        let mut result = parse_file(
            &log,
            source("/project/entry.js", b"import './missing'"),
            Loader::Js,
            &options,
        );
        resolve_import_records(
            &log,
            &file_system,
            &CacheSet::default(),
            &options,
            None,
            &mut result,
        );
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data.text, "Could not resolve \"./missing\"");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn plugins_resolve_and_load_virtual_modules_with_plugin_data() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import value from 'virtual:answer'; console.log(value)".into(),
                ),
                ("/project/dep.js".into(), "export const base = 41".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let resolve_data: config::PluginData = Arc::new("resolve-data".to_string());
        let load_data: config::PluginData = Arc::new("load-data".to_string());
        let saw_resolve_to_load = Arc::new(AtomicBool::new(false));
        let saw_load_to_resolve = Arc::new(AtomicBool::new(false));
        let load_fallthrough_count = Arc::new(AtomicUsize::new(0));

        let plugin = Plugin {
            name: "virtual".into(),
            on_resolve: vec![
                OnResolve {
                    filter: Some(
                        compile_filter_for_plugin("virtual", "OnResolve", "^virtual:")
                            .expect("valid filter"),
                    ),
                    callback: Some({
                        let resolve_data = resolve_data.clone();
                        Arc::new(move |args| {
                            assert_eq!(args.kind, ImportKind::Stmt);
                            assert_eq!(args.path, "virtual:answer");
                            assert_eq!(args.importer.text, "/project/entry.js");
                            assert_eq!(args.importer.namespace, "file");
                            OnResolveResult {
                                path: Path {
                                    text: "answer".into(),
                                    namespace: "virtual".into(),
                                    ..Path::default()
                                },
                                plugin_data: Some(resolve_data.clone()),
                                ..OnResolveResult::default()
                            }
                        })
                    }),
                    name: "virtual".into(),
                    ..OnResolve::default()
                },
                OnResolve {
                    filter: Some(
                        compile_filter_for_plugin("virtual", "OnResolve", r"^\./dep$")
                            .expect("valid filter"),
                    ),
                    callback: Some({
                        let saw_load_to_resolve = saw_load_to_resolve.clone();
                        Arc::new(move |args| {
                            let received = args
                                .plugin_data
                                .as_deref()
                                .and_then(|data| data.downcast_ref::<String>());
                            saw_load_to_resolve.store(
                                received.is_some_and(|value| value == "load-data"),
                                Ordering::SeqCst,
                            );
                            OnResolveResult::default()
                        })
                    }),
                    name: "virtual".into(),
                    namespace: "virtual".into(),
                },
            ],
            on_load: vec![
                OnLoad {
                    filter: Some(
                        compile_filter_for_plugin("virtual", "OnLoad", "^answer$")
                            .expect("valid filter"),
                    ),
                    callback: Some({
                        let load_fallthrough_count = load_fallthrough_count.clone();
                        Arc::new(move |_| {
                            load_fallthrough_count.fetch_add(1, Ordering::SeqCst);
                            OnLoadResult::default()
                        })
                    }),
                    name: "virtual".into(),
                    namespace: "virtual".into(),
                },
                OnLoad {
                    filter: Some(
                        compile_filter_for_plugin("virtual", "OnLoad", "^answer$")
                            .expect("valid filter"),
                    ),
                    callback: Some({
                        let saw_resolve_to_load = saw_resolve_to_load.clone();
                        let load_data = load_data.clone();
                        Arc::new(move |args| {
                            let received = args
                                .plugin_data
                                .as_deref()
                                .and_then(|data| data.downcast_ref::<String>());
                            saw_resolve_to_load.store(
                                received.is_some_and(|value| value == "resolve-data"),
                                Ordering::SeqCst,
                            );
                            OnLoadResult {
                                contents: Some(
                                    "import {base} from './dep'; export default base + 1".into(),
                                ),
                                abs_resolve_dir: "/project".into(),
                                plugin_data: Some(load_data.clone()),
                                loader: Loader::Js,
                                ..OnLoadResult::default()
                            }
                        })
                    }),
                    name: "virtual".into(),
                    namespace: "virtual".into(),
                },
            ],
            ..Plugin::default()
        };
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            extension_order: vec![".js".into()],
            plugins: vec![plugin],
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        let messages = log.done();
        assert!(
            messages.is_empty(),
            "{:?}",
            messages
                .iter()
                .map(|message| &message.data.text)
                .collect::<Vec<_>>()
        );
        assert!(saw_resolve_to_load.load(Ordering::SeqCst));
        assert!(saw_load_to_resolve.load(Ordering::SeqCst));
        assert_eq!(load_fallthrough_count.load(Ordering::SeqCst), 1);
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("console.log"), "{output}");
        assert!(output.contains("41"), "{output}");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn plugins_intercept_entry_points_and_validate_file_namespace_paths() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let saw_entry = Arc::new(AtomicBool::new(false));
        let plugin = Plugin {
            name: "entry".into(),
            on_resolve: vec![OnResolve {
                filter: Some(
                    compile_filter_for_plugin("entry", "OnResolve", "^virtual-entry$")
                        .expect("valid filter"),
                ),
                callback: Some({
                    let saw_entry = saw_entry.clone();
                    Arc::new(move |args| {
                        saw_entry.store(
                            args.kind == ImportKind::EntryPoint
                                && args.importer.namespace.is_empty(),
                            Ordering::SeqCst,
                        );
                        OnResolveResult {
                            path: Path {
                                text: "entry".into(),
                                namespace: "virtual".into(),
                                ..Path::default()
                            },
                            ..OnResolveResult::default()
                        }
                    })
                }),
                name: "entry".into(),
                ..OnResolve::default()
            }],
            on_load: vec![OnLoad {
                filter: Some(
                    compile_filter_for_plugin("entry", "OnLoad", "^entry$").expect("valid filter"),
                ),
                callback: Some(Arc::new(|_| OnLoadResult {
                    contents: Some("console.log('virtual entry')".into()),
                    loader: Loader::Js,
                    ..OnLoadResult::default()
                })),
                name: "entry".into(),
                namespace: "virtual".into(),
            }],
            ..Plugin::default()
        };
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            plugins: vec![plugin],
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "virtual-entry".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        assert!(log.done().is_empty());
        assert!(saw_entry.load(Ordering::SeqCst));
        assert!(
            String::from_utf8_lossy(&compiled.output_files[0].contents).contains("virtual entry")
        );
        assert_eq!(compiled.output_files[0].abs_path, "/out/virtual-entry.js");

        let invalid_log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut invalid_options = Options {
            mode: Mode::Bundle,
            plugins: vec![Plugin {
                name: "invalid".into(),
                on_resolve: vec![OnResolve {
                    filter: Some(
                        compile_filter_for_plugin("invalid", "OnResolve", "^bad$")
                            .expect("valid filter"),
                    ),
                    callback: Some(Arc::new(|_| OnResolveResult {
                        path: Path {
                            text: "relative.js".into(),
                            ..Path::default()
                        },
                        ..OnResolveResult::default()
                    })),
                    name: "invalid".into(),
                    ..OnResolve::default()
                }],
                ..Plugin::default()
            }],
            ..Options::default()
        };
        let _ = scan_bundle(
            &invalid_log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "bad".into(),
                input_path_in_file_namespace: false,
                ..super::EntryPoint::default()
            }],
            &mut invalid_options,
            "TEST",
        );
        let messages = invalid_log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].data.text,
            "Plugin \"invalid\" returned a non-absolute path: relative.js (set a namespace if this is not a file path)"
        );
    }

    #[test]
    fn plugin_external_results_preserve_paths_and_side_effect_metadata() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let options = Options {
            mode: Mode::Bundle,
            plugins: vec![Plugin {
                name: "external".into(),
                on_resolve: vec![OnResolve {
                    filter: Some(
                        compile_filter_for_plugin("external", "OnResolve", "^pkg$")
                            .expect("valid filter"),
                    ),
                    callback: Some(Arc::new(|_| OnResolveResult {
                        external: true,
                        is_side_effect_free: true,
                        ..OnResolveResult::default()
                    })),
                    name: "external".into(),
                    ..OnResolve::default()
                }],
                ..Plugin::default()
            }],
            ..Options::default()
        };
        let mut result = parse_file(
            &log,
            source("/project/entry.js", b"import 'pkg'"),
            Loader::Js,
            &options,
        );
        resolve_import_records(
            &log,
            &file_system,
            &CacheSet::default(),
            &options,
            None,
            &mut result,
        );
        let record = &result
            .file
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("import records")[0];
        assert_eq!(record.path.text, "pkg");
        assert!(
            record
                .flags
                .contains(ImportRecordFlags::IS_EXTERNAL_WITHOUT_SIDE_EFFECTS)
        );
        assert!(
            result.resolve_results[0]
                .as_ref()
                .is_some_and(|result| result.path_pair.is_external)
        );
        assert!(log.done().is_empty());
    }

    #[test]
    fn plugins_substitute_external_require_resolve_paths() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let options = Options {
            mode: Mode::Bundle,
            plugins: vec![Plugin {
                name: "external".into(),
                on_resolve: vec![OnResolve {
                    filter: Some(
                        compile_filter_for_plugin("external", "OnResolve", "^pkg$")
                            .expect("valid filter"),
                    ),
                    callback: Some(Arc::new(|args| {
                        assert_eq!(args.kind, ImportKind::RequireResolve);
                        OnResolveResult {
                            path: Path {
                                text: "replacement".into(),
                                ..Path::default()
                            },
                            external: true,
                            ..OnResolveResult::default()
                        }
                    })),
                    name: "external".into(),
                    ..OnResolve::default()
                }],
                ..Plugin::default()
            }],
            ..Options::default()
        };
        let mut result = parse_file(
            &log,
            source("/project/entry.js", b"require.resolve('pkg')"),
            Loader::Js,
            &options,
        );
        resolve_import_records(
            &log,
            &file_system,
            &CacheSet::default(),
            &options,
            None,
            &mut result,
        );
        let records = result
            .file
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("import records");
        assert_eq!(records[0].path.text, "replacement");
        assert!(
            result.resolve_results[0]
                .as_ref()
                .is_some_and(|result| result.path_pair.is_external)
        );
        assert!(log.done().is_empty());
    }

    #[test]
    fn repeated_unresolved_imports_are_resolved_and_logged_once() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let callback_count = Arc::new(AtomicUsize::new(0));
        let options = Options {
            mode: Mode::Bundle,
            plugins: vec![Plugin {
                name: "counter".into(),
                on_resolve: vec![OnResolve {
                    filter: Some(
                        compile_filter_for_plugin("counter", "OnResolve", "^missing$")
                            .expect("valid filter"),
                    ),
                    callback: Some({
                        let callback_count = callback_count.clone();
                        Arc::new(move |_| {
                            callback_count.fetch_add(1, Ordering::SeqCst);
                            OnResolveResult::default()
                        })
                    }),
                    name: "counter".into(),
                    ..OnResolve::default()
                }],
                ..Plugin::default()
            }],
            ..Options::default()
        };
        let mut result = parse_file(
            &log,
            source(
                "/project/entry.js",
                b"import 'missing'; import value from 'missing'; console.log(value)",
            ),
            Loader::Js,
            &options,
        );
        resolve_import_records(
            &log,
            &file_system,
            &CacheSet::default(),
            &options,
            None,
            &mut result,
        );
        let messages = log.done();
        assert_eq!(callback_count.load(Ordering::SeqCst), 1);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data.text, "Could not resolve \"missing\"");
    }

    #[test]
    fn virtual_modules_without_resolve_directories_cannot_resolve_relative_imports() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([("dep.js".into(), "export default 123".into())]),
            MockKind::Unix,
            "/project",
        );
        let plugin = Plugin {
            name: "virtual".into(),
            on_resolve: vec![OnResolve {
                filter: Some(
                    compile_filter_for_plugin("virtual", "OnResolve", "^virtual-entry$")
                        .expect("valid filter"),
                ),
                callback: Some(Arc::new(|_| OnResolveResult {
                    path: Path {
                        text: "entry".into(),
                        namespace: "virtual".into(),
                        ..Path::default()
                    },
                    ..OnResolveResult::default()
                })),
                name: "virtual".into(),
                ..OnResolve::default()
            }],
            on_load: vec![OnLoad {
                filter: Some(
                    compile_filter_for_plugin("virtual", "OnLoad", "^entry$")
                        .expect("valid filter"),
                ),
                callback: Some(Arc::new(|_| OnLoadResult {
                    contents: Some("import './dep.js'".into()),
                    loader: Loader::Js,
                    ..OnLoadResult::default()
                })),
                name: "virtual".into(),
                namespace: "virtual".into(),
            }],
            ..Plugin::default()
        };
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into()],
            plugins: vec![plugin],
            ..Options::default()
        };
        let _ = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "virtual-entry".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data.text, "Could not resolve \"./dep.js\"");
    }

    #[test]
    fn plugin_errors_stop_resolution_without_duplicate_generic_errors() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let later_callback_count = Arc::new(AtomicUsize::new(0));
        let options = Options {
            mode: Mode::Bundle,
            plugins: vec![Plugin {
                name: "broken".into(),
                on_resolve: vec![
                    OnResolve {
                        filter: Some(
                            compile_filter_for_plugin("broken", "OnResolve", "^pkg$")
                                .expect("valid filter"),
                        ),
                        callback: Some(Arc::new(|_| OnResolveResult {
                            messages: vec![crate::internal::logger::Msg::new(
                                crate::internal::logger::MsgKind::Error,
                                "plugin failure",
                            )],
                            ..OnResolveResult::default()
                        })),
                        name: "broken".into(),
                        ..OnResolve::default()
                    },
                    OnResolve {
                        filter: Some(
                            compile_filter_for_plugin("broken", "OnResolve", "^pkg$")
                                .expect("valid filter"),
                        ),
                        callback: Some({
                            let later_callback_count = later_callback_count.clone();
                            Arc::new(move |_| {
                                later_callback_count.fetch_add(1, Ordering::SeqCst);
                                OnResolveResult::default()
                            })
                        }),
                        name: "broken".into(),
                        ..OnResolve::default()
                    },
                ],
                ..Plugin::default()
            }],
            ..Options::default()
        };
        let mut result = parse_file(
            &log,
            source("/project/entry.js", b"import 'pkg'"),
            Loader::Js,
            &options,
        );
        resolve_import_records(
            &log,
            &file_system,
            &CacheSet::default(),
            &options,
            None,
            &mut result,
        );
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data.text, "plugin failure");
        assert_eq!(messages[0].plugin_name, "broken");
        assert!(messages[0].data.location.is_some());
        assert_eq!(later_callback_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn plugin_load_errors_point_to_the_triggering_import() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([(
                "/project/entry.js".into(),
                "import value from 'virtual'; console.log(value)".into(),
            )]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into()],
            plugins: vec![Plugin {
                name: "broken-load".into(),
                on_resolve: vec![OnResolve {
                    filter: Some(
                        compile_filter_for_plugin("broken-load", "OnResolve", "^virtual$")
                            .expect("valid filter"),
                    ),
                    callback: Some(Arc::new(|_| OnResolveResult {
                        path: Path {
                            text: "module".into(),
                            namespace: "virtual".into(),
                            ..Path::default()
                        },
                        ..OnResolveResult::default()
                    })),
                    name: "broken-load".into(),
                    ..OnResolve::default()
                }],
                on_load: vec![OnLoad {
                    filter: Some(
                        compile_filter_for_plugin("broken-load", "OnLoad", "^module$")
                            .expect("valid filter"),
                    ),
                    callback: Some(Arc::new(|_| OnLoadResult {
                        messages: vec![Msg::new(MsgKind::Error, "load failure")],
                        ..OnLoadResult::default()
                    })),
                    name: "broken-load".into(),
                    namespace: "virtual".into(),
                }],
                ..Plugin::default()
            }],
            ..Options::default()
        };
        let _ = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].data.text, "load failure");
        assert_eq!(messages[0].plugin_name, "broken-load");
        let location = messages[0]
            .data
            .location
            .as_ref()
            .expect("triggering import location");
        assert_eq!(location.file.rel, "entry.js");
        assert_eq!(
            String::from_utf8_lossy(&location.line_text),
            "import value from 'virtual'; console.log(value)"
        );
    }

    #[test]
    fn bundles_filesystem_entry_points_to_output_files() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([("/project/entry.js".into(), "console.log('bundled')".into())]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(
            compiled.scan_result.import_issues.is_empty(),
            "{:?}",
            compiled.scan_result.import_issues
        );
        assert_eq!(compiled.output_files.len(), 1);
        assert_eq!(compiled.output_files[0].abs_path, "/out/entry.js");
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("console.log(\"bundled\");"));
        assert!(output.starts_with("(() => {\n"));
        assert!(output.ends_with("})();\n"));
    }

    #[test]
    fn bundles_css_entry_points_to_output_files() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([("/project/entry.css".into(), ".entry { color: red }".into())]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.css".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert_eq!(compiled.output_files.len(), 1);
        assert!(compiled.output_files[0].abs_path.ends_with("/entry.css"));
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains(".entry"));
        assert!(output.contains("color: red"));
    }

    #[test]
    fn emits_css_imported_by_javascript_entries() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import './style.css'; console.log('entry')".into(),
                ),
                ("/project/style.css".into(), ".style { color: blue }".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::EsModule,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert_eq!(compiled.output_files.len(), 2);
        let javascript = compiled
            .output_files
            .iter()
            .find(|output| output.abs_path.ends_with("/entry.js"))
            .expect("JavaScript output");
        let css = compiled
            .output_files
            .iter()
            .find(|output| output.abs_path.ends_with("/entry.css"))
            .expect("CSS output");
        assert!(String::from_utf8_lossy(&javascript.contents).contains("console.log(\"entry\")"));
        assert!(!String::from_utf8_lossy(&javascript.contents).contains("style.css"));
        assert!(!String::from_utf8_lossy(&javascript.contents).contains("import "));
        assert!(String::from_utf8_lossy(&css.contents).contains("color: blue"));
    }

    #[test]
    fn bundles_nested_css_imported_by_a_javascript_entry() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import './style.css'; console.log('entry')".into(),
                ),
                (
                    "/project/style.css".into(),
                    "@import './theme.css'; .style { color: blue }".into(),
                ),
                (
                    "/project/theme.css".into(),
                    ":root { --accent: orange }".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::EsModule,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert_eq!(compiled.output_files.len(), 2);
        let css = compiled
            .output_files
            .iter()
            .find(|output| output.abs_path.ends_with("/entry.css"))
            .expect("CSS output");
        let output = String::from_utf8_lossy(&css.contents);
        let theme_offset = output.find(":root").expect("nested theme CSS");
        let style_offset = output.find(".style").expect("importing stylesheet CSS");
        assert!(theme_offset < style_offset, "{output}");
        assert!(output.contains("--accent: orange"), "{output}");
        assert!(output.contains("color: blue"), "{output}");
        assert!(!output.contains("@import"), "{output}");
    }

    #[test]
    fn bundles_named_imports_across_javascript_modules() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import { value } from './dep.js'; console.log(value)".into(),
                ),
                ("/project/dep.js".into(), "export const value = 123".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(
            compiled.scan_result.import_issues.is_empty(),
            "{:?}",
            compiled.scan_result.import_issues
        );
        assert_eq!(compiled.output_files.len(), 1);
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("var value = 123;"));
        assert!(output.contains("console.log(value);"));
        assert!(!output.contains("import "));
        assert!(!output.contains("export "));
    }

    #[test]
    fn inlines_imported_constants_after_tree_shaking_their_declarations() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import { scale } from './scale.js'; console.log(scale(2))".into(),
                ),
                (
                    "/project/scale.js".into(),
                    "import { durationSecond } from './duration.js'; export function scale(value) { return value * durationSecond }"
                        .into(),
                ),
                (
                    "/project/duration.js".into(),
                    "export const durationSecond = 1000".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            tree_shaking: true,
            minify_syntax: true,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("return value * 1e3;"), "{output}");
        assert!(!output.contains("durationSecond"), "{output}");
    }

    #[test]
    fn tree_shakes_unused_top_level_statements_in_real_bundles() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import { used } from './dep.js'; const deadEntry = 1, liveEntry = used; console.log(liveEntry)"
                        .into(),
                ),
                (
                    "/project/dep.js".into(),
                    "const deadDependency = 2, used = 3; console.log('dependency effect'); export { used, deadDependency }"
                        .into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            tree_shaking: true,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(
            compiled.scan_result.import_issues.is_empty(),
            "{:?}",
            compiled.scan_result.import_issues
        );
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("var used = 3;"));
        assert!(output.contains("console.log(\"dependency effect\");"));
        assert!(output.contains("var liveEntry = used;"));
        assert!(output.contains("console.log(liveEntry);"));
        assert!(!output.contains("deadEntry"));
        assert!(!output.contains("deadDependency"));
    }

    #[test]
    fn tree_shakes_dead_jsx_away_from_shared_generated_imports() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.jsx".into(),
                    "const dead = <div />; console.log(<span />)".into(),
                ),
                (
                    "/project/node_modules/react/package.json".into(),
                    r#"{"main":"index.js","sideEffects":false}"#.into(),
                ),
                (
                    "/project/node_modules/react/jsx-runtime.js".into(),
                    "export function jsx(type, props) { return {type, props} }".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            tree_shaking: true,
            jsx: crate::internal::config::JsxOptions {
                automatic_runtime: true,
                ..crate::internal::config::JsxOptions::default()
            },
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.jsx".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(
            compiled.scan_result.import_issues.is_empty(),
            "{:?}",
            compiled.scan_result.import_issues
        );
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(!output.contains("dead"));
        assert!(output.contains("function jsx(type, props)"));
        assert!(output.contains("console.log(/* @__PURE__ */ jsx(\"span\", {}));"));
    }

    #[test]
    fn renames_colliding_top_level_symbols_across_modules() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import {value as a} from './a.js'; import {value as b} from './b.js'; console.log(a, b)".into(),
                ),
                (
                    "/project/a.js".into(),
                    "const collision = 1; export {collision as value}".into(),
                ),
                (
                    "/project/b.js".into(),
                    "const collision = 2; export {collision as value}".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("var collision = 1;"));
        assert!(output.contains("var collision2 = 2;"));
        assert!(output.contains("console.log(collision, collision2);"));
    }

    #[test]
    fn minifies_identifiers_in_javascript_bundles() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([(
                "/project/entry.js".into(),
                "function longFunction(longParameter) { let longLocal = longParameter + 1; return longLocal } console.log(longFunction(2))".into(),
            )]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            minify_identifiers: true,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(!output.contains("longFunction"));
        assert!(!output.contains("longParameter"));
        assert!(!output.contains("longLocal"));
        assert!(output.contains("console.log("));
    }

    #[test]
    fn bundles_common_js_require_dependencies() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "const dep = require('./dep.js'); console.log(dep.value)".into(),
                ),
                ("/project/dep.js".into(), "exports.value = 123".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(compiled.scan_result.import_issues.is_empty());
        assert_eq!(compiled.output_files.len(), 1);
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("require_dep"));
        assert!(output.contains("exports.value = 123;"));
        assert!(output.contains("var dep = require_dep();"));
        assert!(output.contains("console.log(dep.value);"));
        assert!(!output.contains("require(\"./dep.js\")"));
    }

    #[test]
    fn splits_dynamic_imports_into_esm_chunks() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import('./dep.js').then(ns => console.log(ns.value))".into(),
                ),
                ("/project/dep.js".into(), "export const value = 123".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::EsModule,
            code_splitting: true,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(compiled.scan_result.import_issues.is_empty());
        assert_eq!(compiled.output_files.len(), 2);
        let entry = compiled
            .output_files
            .iter()
            .find(|output| output.abs_path.ends_with("/entry.js"))
            .expect("entry output");
        let dependency = compiled
            .output_files
            .iter()
            .find(|output| output.abs_path != entry.abs_path)
            .expect("dependency chunk");
        let entry_code = String::from_utf8_lossy(&entry.contents);
        let dependency_code = String::from_utf8_lossy(&dependency.contents);
        assert!(
            entry_code.contains("import(\"./dep-"),
            "entry:\n{entry_code}\ndependency:\n{dependency_code}"
        );
        assert!(!entry_code.contains("TESTC"));
        assert!(dependency.abs_path.contains("/dep-"));
        assert!(dependency_code.contains("var value = 123;"));
        assert!(dependency_code.contains("export {"));
        assert!(dependency_code.contains("value"));
    }

    #[test]
    fn splits_shared_dependencies_between_esm_entries() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/a.js".into(),
                    "import {value} from './shared.js'; console.log('a', value)".into(),
                ),
                (
                    "/project/b.js".into(),
                    "import {value} from './shared.js'; console.log('b', value)".into(),
                ),
                (
                    "/project/shared.js".into(),
                    "export const value = 123".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::EsModule,
            code_splitting: true,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[
                super::EntryPoint {
                    input_path: "a.js".into(),
                    ..super::EntryPoint::default()
                },
                super::EntryPoint {
                    input_path: "b.js".into(),
                    ..super::EntryPoint::default()
                },
            ],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(
            compiled.scan_result.import_issues.is_empty(),
            "{:?}",
            compiled.scan_result.import_issues
        );
        assert_eq!(compiled.output_files.len(), 3);
        let shared = compiled
            .output_files
            .iter()
            .find(|output| output.abs_path.contains("/chunk-"))
            .expect("shared chunk");
        let shared_name = shared
            .abs_path
            .rsplit('/')
            .next()
            .expect("shared chunk basename");
        let shared_code = String::from_utf8_lossy(&shared.contents);
        assert!(shared_code.contains("var value = 123;"));
        assert!(shared_code.contains("export {"));
        for entry_name in ["a.js", "b.js"] {
            let entry = compiled
                .output_files
                .iter()
                .find(|output| output.abs_path.ends_with(entry_name))
                .expect("entry output");
            let entry_code = String::from_utf8_lossy(&entry.contents);
            assert!(
                entry_code.contains(&format!("from \"./{shared_name}\"")),
                "{entry_name}:\n{entry_code}\nshared:\n{shared_code}"
            );
            assert!(!entry_code.contains("TESTC"));
        }
    }

    #[test]
    fn converts_esm_entry_exports_to_common_js() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([(
                "/project/entry.js".into(),
                "export const value = 123".into(),
            )]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::CommonJs,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(compiled.scan_result.import_issues.is_empty());
        assert_eq!(compiled.output_files.len(), 1);
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("__export(entry_exports"));
        assert!(output.contains("value: () => value"));
        assert!(output.contains("var value = 123;"));
        assert!(output.contains("module.exports = __toCommonJS(entry_exports);"));
        assert!(!output.contains("export const"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn scans_entry_points_into_a_recursive_module_graph() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import './dep'; import './style.css'".into(),
                ),
                (
                    "/project/dep.js".into(),
                    "import './entry.js'; export const value = 1".into(),
                ),
                (
                    "/project/style.css".into(),
                    "@import './nested.css'; .entry { color: red }".into(),
                ),
                (
                    "/project/nested.css".into(),
                    ".nested { color: blue }".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let caches = CacheSet::default();
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into(), ".css".into()],
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &caches,
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                output_path: "app".into(),
                input_path_in_file_namespace: true,
            }],
            &mut options,
            "TEST",
        );

        assert_eq!(bundle.entry_points.len(), 1);
        assert_eq!(bundle.entry_points[0].output_path, "app");
        assert_eq!(bundle.files.len(), 6);
        assert_eq!(
            bundle.files[runtime::SOURCE_INDEX as usize]
                .input_file
                .source
                .key_path
                .text,
            "<runtime>"
        );
        assert!(
            bundle.files[runtime::SOURCE_INDEX as usize]
                .input_file
                .omit_from_source_maps_and_metafile
        );

        let loaded_paths = bundle
            .files
            .iter()
            .skip(1)
            .map(|file| file.input_file.source.key_path.text.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(
            loaded_paths,
            HashSet::from([
                "/project/entry.js",
                "/project/dep.js",
                "/project/style.css",
                "/project/nested.css",
            ])
        );
        let css_file = bundle
            .files
            .iter()
            .find(|file| {
                file.input_file.source.key_path.text == "/project/style.css"
                    && matches!(file.input_file.repr, Some(InputFileRepr::Css(_)))
            })
            .expect("stylesheet input");
        let Some(InputFileRepr::Css(css_repr)) = &css_file.input_file.repr else {
            panic!("expected stylesheet representation");
        };
        assert!(css_repr.js_source_index.is_valid());
        let stub = &bundle.files[css_repr.js_source_index.get_index() as usize];
        let Some(InputFileRepr::Js(stub_repr)) = &stub.input_file.repr else {
            panic!("expected JavaScript CSS stub");
        };
        assert_eq!(
            stub_repr.css_source_index.get_index(),
            css_file.input_file.source.index
        );
        let entry = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text == "/project/entry.js")
            .expect("entry input");
        let style_import = entry
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("entry import records")
            .iter()
            .find(|record| record.path.text == "./style.css")
            .expect("style import");
        assert_eq!(
            style_import.source_index.get_index(),
            css_repr.js_source_index.get_index()
        );
        for file in bundle.files.iter().skip(1) {
            for record in file
                .input_file
                .repr
                .as_ref()
                .and_then(InputFileRepr::import_records)
                .unwrap_or_default()
            {
                assert!(record.source_index.is_valid());
            }
        }
        assert!(log.done().is_empty());
    }

    #[test]
    fn scan_applies_nearest_tsconfig_paths() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/tsconfig.json".into(),
                    r#"{"extends":"config-base/base.json"}"#.into(),
                ),
                (
                    "/project/node_modules/config-base/base.json".into(),
                    r#"{"compilerOptions":{"baseUrl":"../..","paths":{"@lib/*":["src/lib/*"]}}}"#
                        .into(),
                ),
                (
                    "/project/src/entry.ts".into(),
                    "import { value } from '@lib/value'; const result: number = value; console.log(result)"
                        .into(),
                ),
                (
                    "/project/src/lib/value.ts".into(),
                    "export const value: number = 42".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            tree_shaking: true,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            ..Options::default()
        };
        let compiled = bundle_javascript(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "src/entry.ts".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );

        assert!(log.done().is_empty());
        assert!(
            compiled.scan_result.import_issues.is_empty(),
            "{:?}",
            compiled.scan_result.import_issues
        );
        let output = String::from_utf8_lossy(&compiled.output_files[0].contents);
        assert!(output.contains("var value = 42;"), "{output}");
        assert!(output.contains("var result = value;"), "{output}");
        assert!(output.contains("console.log(result);"), "{output}");
    }

    #[test]
    fn reports_missing_entry_points_during_bundle_scanning() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into()],
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "/project/missing.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        assert!(bundle.entry_points.is_empty());
        assert_eq!(bundle.files.len(), 1);
        assert_eq!(
            log.done()[0].data.text,
            "Could not resolve \"/project/missing.js\""
        );
    }

    #[test]
    fn scan_applies_package_metadata_before_parsing() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import 'metadata-package'".into(),
                ),
                (
                    "/project/node_modules/metadata-package/package.json".into(),
                    r#"{"main":"index.js","sideEffects":false,"type":"module"}"#.into(),
                ),
                (
                    "/project/node_modules/metadata-package/index.js".into(),
                    "console.log('loaded')".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into()],
            platform: Platform::Node,
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        let package = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text.ends_with("/index.js"))
            .expect("package input");
        assert_eq!(
            package.input_file.side_effects.kind,
            SideEffectsKind::NoSideEffectsPackageJson
        );
        assert!(package.input_file.side_effects.data.is_some());
        let Some(InputFileRepr::Js(repr)) = &package.input_file.repr else {
            panic!("expected package JavaScript");
        };
        assert_eq!(repr.ast.exports_kind, ExportsKind::Esm);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Warning);
        assert!(
            messages[0]
                .data
                .text
                .starts_with("Ignoring this import because ")
        );
        assert_eq!(messages[0].notes.len(), 1);
    }

    #[test]
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        clippy::too_many_lines
    )]
    fn scan_avoids_dual_package_hazards_and_relocates_copy_imports() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import value from 'dual'; require('dual'); import './asset.txt'; import logo from './logo.png'; console.log(logo)".into(),
                ),
                (
                    "/project/node_modules/dual/package.json".into(),
                    r#"{"main":"main.js","module":"module.js"}"#.into(),
                ),
                (
                    "/project/node_modules/dual/main.js".into(),
                    "module.exports = 1".into(),
                ),
                (
                    "/project/node_modules/dual/module.js".into(),
                    "export default 1".into(),
                ),
                ("/project/asset.txt".into(), "asset".into()),
                ("/project/logo.png".into(), "png".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into()],
            platform: Platform::Browser,
            abs_output_dir: "/out".into(),
            abs_output_base: "/project".into(),
            needs_metafile: true,
            extension_to_loader: HashMap::from([
                (".js".into(), Loader::Js),
                (".txt".into(), Loader::Copy),
                (".png".into(), Loader::File),
            ]),
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        let entry = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text == "/project/entry.js")
            .expect("entry input");
        let records = entry
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("entry import records");
        let main_index = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text.ends_with("/main.js"))
            .expect("CommonJS package path")
            .input_file
            .source
            .index;
        let dual_records = records
            .iter()
            .filter(|record| record.path.text == "dual")
            .collect::<Vec<_>>();
        assert_eq!(dual_records.len(), 2);
        assert!(
            dual_records
                .iter()
                .all(|record| record.source_index.get_index() == main_index)
        );
        let asset_record = records
            .iter()
            .find(|record| record.path.text == "./asset.txt")
            .expect("copy import");
        assert!(!asset_record.source_index.is_valid());
        assert!(asset_record.copy_source_index.is_valid());
        let copied = &bundle.files[asset_record.copy_source_index.get_index() as usize];
        assert!(matches!(
            copied.input_file.repr,
            Some(InputFileRepr::Copy(_))
        ));
        assert_eq!(copied.input_file.additional_files.len(), 1);
        let copied_output = &copied.input_file.additional_files[0];
        assert!(copied_output.abs_path.starts_with("/out/asset-"));
        assert!(copied_output.abs_path.ends_with(".txt"));
        assert_eq!(copied_output.contents, b"asset");
        assert!(copied_output.json_metadata_chunk.contains("\"bytes\": 5"));

        let logo_record = records
            .iter()
            .find(|record| record.path.text == "./logo.png")
            .expect("file loader import");
        let logo = &bundle.files[logo_record.source_index.get_index() as usize];
        assert_eq!(logo.input_file.loader, Loader::File);
        assert_eq!(logo.input_file.additional_files.len(), 1);
        let logo_output = &logo.input_file.additional_files[0];
        assert!(logo_output.abs_path.starts_with("/out/logo-"));
        assert!(logo_output.abs_path.ends_with(".png"));
        assert_eq!(logo_output.contents, b"png");
        assert!(log.done().is_empty());
    }

    #[test]
    fn scan_rejects_javascript_css_imports_when_writing_to_stdout() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                ("/project/entry.js".into(), "import './style.css'".into()),
                ("/project/style.css".into(), ".entry { color: red }".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into(), ".css".into()],
            write_to_stdout: true,
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        assert_eq!(bundle.files.len(), 3);
        let entry = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text == "/project/entry.js")
            .expect("entry input");
        let record = &entry
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("entry import records")[0];
        assert!(!record.source_index.is_valid());
        assert_eq!(
            log.done()[0].data.text,
            "Cannot import \"style.css\" into a JavaScript file without an output path configured"
        );
    }

    #[test]
    fn copy_loader_entries_honor_explicit_output_files() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([("/project/asset.txt".into(), "entry asset".into())]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".txt".into()],
            extension_to_loader: HashMap::from([(".txt".into(), Loader::Copy)]),
            abs_output_base: "/project".into(),
            abs_output_dir: "/out".into(),
            abs_output_file: "/out/single.bin".into(),
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "asset.txt".into(),
                output_path: "ignored/custom-name".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        let asset = bundle
            .files
            .iter()
            .find(|file| file.input_file.loader == Loader::Copy)
            .expect("copy entry");
        assert_eq!(asset.input_file.additional_files.len(), 1);
        assert_eq!(
            asset.input_file.additional_files[0].abs_path,
            "/out/single.bin"
        );
        assert_eq!(
            asset.input_file.additional_files[0].contents,
            b"entry asset"
        );
        assert!(log.done().is_empty());
    }

    #[test]
    fn scan_validates_css_import_target_types() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/style.module.css".into(),
                    "@import './script.js'; .foo { composes: bar from './script.js' } .image { background: url(./other.css) }".into(),
                ),
                (
                    "/project/script.js".into(),
                    "export const value = 1".into(),
                ),
                (
                    "/project/other.css".into(),
                    ".other { color: red }".into(),
                ),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".css".into(), ".js".into()],
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "style.module.css".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        assert_eq!(bundle.files.len(), 4);
        let messages = log.done();
        assert_eq!(messages.len(), 3);
        let texts = messages
            .iter()
            .map(|message| message.data.text.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(
            texts,
            HashSet::from([
                "Cannot import \"script.js\" into a CSS file",
                "Cannot use \"composes\" with \"script.js\"",
                "Cannot use \"other.css\" as a URL",
            ])
        );
    }

    #[test]
    fn scan_applies_import_attributes_and_validates_json_assertions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "import textJson from './data.txt' with { type: 'json' };\
                     import('./data.txt', { with: { type: 'json' } });\
                     import json from './data.json' assert { type: 'json' };\
                     import bad from './code.js' assert { type: 'json' };\
                     console.log(textJson, json, bad)"
                        .into(),
                ),
                ("/project/data.txt".into(), r#"{"text":true}"#.into()),
                ("/project/data.json".into(), r#"{"json":true}"#.into()),
                ("/project/code.js".into(), "export default 1".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into(), ".json".into(), ".txt".into()],
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        let text_json = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text == "/project/data.txt")
            .expect("JSON imported through an attribute");
        assert_eq!(text_json.input_file.loader, Loader::WithTypeJson);
        assert_eq!(
            text_json
                .input_file
                .source
                .key_path
                .import_attributes
                .decode_into_map()
                .get("type")
                .map(String::as_str),
            Some("json")
        );
        let Some(InputFileRepr::Js(text_json_repr)) = &text_json.input_file.repr else {
            panic!("expected lazy JSON JavaScript representation");
        };
        assert_eq!(text_json_repr.ast.exports_kind, ExportsKind::Esm);
        let entry = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text == "/project/entry.js")
            .expect("entry input");
        let dynamic_record = entry
            .input_file
            .repr
            .as_ref()
            .and_then(InputFileRepr::import_records)
            .expect("entry import records")
            .iter()
            .find(|record| record.kind == ImportKind::Dynamic)
            .expect("dynamic import attribute record");
        assert_eq!(
            dynamic_record.source_index.get_index(),
            text_json.input_file.source.index
        );

        let ordinary_json = bundle
            .files
            .iter()
            .find(|file| file.input_file.source.key_path.text == "/project/data.json")
            .expect("ordinary JSON assertion target");
        assert_eq!(ordinary_json.input_file.loader, Loader::Json);

        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].data.text,
            "The file \"code.js\" was loaded with the \"js\" loader"
        );
    }

    #[test]
    fn scan_marks_static_top_level_await_chains_async() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                ("/project/entry.js".into(), "import './middle.js'".into()),
                ("/project/middle.js".into(), "import './async.js'".into()),
                ("/project/async.js".into(), "await Promise.resolve()".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into()],
            ..Options::default()
        };
        let bundle = scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        for path in ["entry.js", "middle.js", "async.js"] {
            let file = bundle
                .files
                .iter()
                .find(|file| file.input_file.source.key_path.text.ends_with(path))
                .expect("JavaScript input");
            let Some(InputFileRepr::Js(repr)) = &file.input_file.repr else {
                panic!("expected JavaScript representation");
            };
            assert!(repr.meta.is_async_or_has_async_dependency, "{path}");
        }
        assert!(log.done().is_empty());
    }

    #[test]
    fn scan_rejects_direct_and_transitive_require_of_top_level_await() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/entry.js".into(),
                    "require('./async.js'); require('./middle.js')".into(),
                ),
                ("/project/middle.js".into(), "import './async.js'".into()),
                ("/project/async.js".into(), "await Promise.resolve()".into()),
            ]),
            MockKind::Unix,
            "/project",
        );
        let mut options = Options {
            mode: Mode::Bundle,
            extension_order: vec![".js".into()],
            ..Options::default()
        };
        scan_bundle(
            &log,
            &file_system,
            &CacheSet::default(),
            &[super::EntryPoint {
                input_path: "entry.js".into(),
                ..super::EntryPoint::default()
            }],
            &mut options,
            "TEST",
        );
        let messages = log.done();
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().any(|message| {
            message.data.text
                == "This require call is not allowed because the imported file \"async.js\" contains a top-level await"
                && message.notes.len() == 1
        }));
        assert!(messages.iter().any(|message| {
            message.data.text
                == "This require call is not allowed because the transitive dependency \"async.js\" contains a top-level await"
                && message.notes.len() == 2
        }));
    }
}
