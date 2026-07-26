//! Port of esbuild's public `pkg/api` package.

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt, fs as std_fs,
    io::{self, Write as _},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use crate::internal::{
    ast::{DEFAULT_NAME_MINIFIER_CSS, DEFAULT_NAME_MINIFIER_JS, Ref, SymbolKind, SymbolMap},
    bundler,
    cache::CacheSet,
    config::{self, Mode},
    css_parser, css_printer,
    fs::{Fs, MockKind, RealFsOptions, mock_fs, real_fs},
    helpers::{
        encode_string_as_shortest_data_url, escape_closing_tag, mime_type_by_extension,
        quote_for_json, string_to_utf16,
    },
    js_ast::generate_non_unique_name_from_path,
    js_parser, js_printer,
    logger::{
        DeferLogKind, Log, Msg, MsgData, MsgKind, MsgLocation, OutputOptions, Path, PrettyPaths,
        Source, TerminalInfo, msg_id_to_string, string_to_maximum_msg_id,
    },
    renamer::{Renamer, new_no_op_renamer},
    resolver,
    sourcemap::{Chunk as SourceMapChunk, LineColumnOffset, generate_line_offset_tables},
    xxhash,
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};

struct TransformRenamer {
    base: Box<dyn Renamer>,
    symbols: SymbolMap,
    overrides: HashMap<Ref, String>,
}

impl Renamer for TransformRenamer {
    fn name_for_symbol(&self, reference: Ref) -> String {
        let reference = self.symbols.follow_symbols_const(reference);
        self.overrides
            .get(&reference)
            .cloned()
            .unwrap_or_else(|| self.base.name_for_symbol(reference))
    }

    fn namespace_alias_for_symbol(
        &self,
        reference: Ref,
    ) -> Option<crate::internal::ast::NamespaceAlias> {
        let reference = self.symbols.follow_symbols_const(reference);
        self.symbols.get(reference).namespace_alias.clone()
    }
}

#[derive(Default)]
struct KeepNameHelper {
    def_prop: String,
    name: String,
    target: String,
    value: String,
}

fn transform_keep_name_renamer(
    ast: &crate::internal::js_ast::Ast,
    symbols: SymbolMap,
    keep_names: bool,
    minify_identifiers: bool,
) -> (TransformRenamer, KeepNameHelper) {
    let mut overrides = HashMap::new();
    let helper_refs = if keep_names {
        ast.named_imports
            .iter()
            .filter_map(|(reference, import)| (import.alias == "__name").then_some(*reference))
            .chain(
                ast.module_scope
                    .as_ref()
                    .into_iter()
                    .flat_map(|scope| {
                        scope
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .generated
                            .clone()
                    })
                    .filter(|reference| {
                        ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                            .original_name
                            == "__name"
                    }),
            )
            .collect::<HashSet<_>>()
    } else {
        HashSet::new()
    };
    let helper_use_count = helper_refs
        .iter()
        .map(|reference| {
            ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                .use_count_estimate
        })
        .sum::<u32>();
    let (base, mut helper) = transform_base_renamer(
        ast,
        &symbols,
        minify_identifiers,
        (!helper_refs.is_empty()).then_some(helper_use_count),
    );
    if !helper_refs.is_empty() {
        if !minify_identifiers {
            let helper_indices = helper_refs
                .iter()
                .map(|reference| reference.inner_index)
                .collect::<HashSet<_>>();
            let used_names = ast
                .symbols
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    !helper_indices.contains(&u32::try_from(*index).expect("symbol index fits u32"))
                })
                .map(|(_, symbol)| symbol.original_name.as_str())
                .collect::<HashSet<_>>();
            helper.name = "__name".into();
            let mut suffix = 2;
            while used_names.contains(helper.name.as_str()) {
                helper.name = format!("__name{suffix}");
                suffix += 1;
            }
            helper.def_prop = "__defProp".into();
            suffix = 2;
            while used_names.contains(helper.def_prop.as_str()) || helper.def_prop == helper.name {
                helper.def_prop = format!("__defProp{suffix}");
                suffix += 1;
            }
            helper.target = "target".into();
            helper.value = "value".into();
        }
        overrides.extend(
            helper_refs
                .into_iter()
                .map(|reference| (reference, helper.name.clone())),
        );
    }
    (
        TransformRenamer {
            base,
            symbols,
            overrides,
        },
        helper,
    )
}

fn transform_base_renamer(
    ast: &crate::internal::js_ast::Ast,
    symbols: &SymbolMap,
    minify_identifiers: bool,
    keep_name_use_count: Option<u32>,
) -> (Box<dyn Renamer>, KeepNameHelper) {
    if minify_identifiers {
        let scopes = ast.module_scope.iter().cloned().collect::<Vec<_>>();
        let reserved_names = crate::internal::renamer::compute_reserved_names(&scopes, symbols);
        let mut renamer = crate::internal::renamer::MinifyRenamer::new(
            symbols.clone(),
            ast.nested_scope_slot_counts,
            reserved_names,
        );
        let mut top_level_symbols = Vec::new();
        for part in &ast.parts {
            renamer.accumulate_symbol_use_counts(&mut top_level_symbols, &part.symbol_uses, &[0]);
            for declared in &part.declared_symbols {
                renamer.accumulate_symbol_count(
                    &mut top_level_symbols,
                    declared.reference,
                    1,
                    &[0],
                );
            }
        }
        let mut imported_symbols = top_level_symbols
            .iter()
            .copied()
            .filter(|stable| {
                symbols.get(stable.reference).kind == crate::internal::ast::SymbolKind::Import
            })
            .collect::<Vec<_>>();
        crate::internal::renamer::sort_stable_symbol_counts(&mut imported_symbols);
        renamer.allocate_top_level_symbol_slots(&imported_symbols);
        let keep_name_slots = keep_name_use_count.map(|use_count| {
            renamer.accumulate_synthetic_default_nested_slot(1, 2);
            renamer.accumulate_synthetic_default_nested_slot(2, 2);
            let def_prop = renamer.allocate_synthetic_default_top_level_slot(2);
            let name = renamer.allocate_synthetic_default_top_level_slot(use_count.wrapping_add(2));
            (def_prop, name)
        });
        let minifier =
            DEFAULT_NAME_MINIFIER_JS.shuffle_by_char_freq(ast.char_freq.unwrap_or_default());
        renamer.assign_names_by_frequency(&minifier);
        let helper = keep_name_slots
            .map(|(def_prop, name)| KeepNameHelper {
                def_prop: renamer.name_for_synthetic_default_slot(def_prop),
                name: renamer.name_for_synthetic_default_slot(name),
                target: renamer.name_for_synthetic_default_slot(1),
                value: renamer.name_for_synthetic_default_slot(2),
            })
            .unwrap_or_default();
        (Box::new(renamer), helper)
    } else {
        let scopes = ast.module_scope.iter().cloned().collect::<Vec<_>>();
        let reserved_names = crate::internal::renamer::compute_reserved_names(&scopes, symbols);
        let mut renamer =
            crate::internal::renamer::NumberRenamer::new(symbols.clone(), reserved_names);
        let mut nested_scopes = Vec::new();
        for part in &ast.parts {
            for declared in &part.declared_symbols {
                if declared.is_top_level {
                    renamer.add_top_level_symbol(declared.reference);
                }
            }
            nested_scopes.extend(part.scopes.iter().cloned());
        }
        renamer.assign_names_by_scope(&HashMap::from([(0, nested_scopes)]));
        (Box::new(renamer), KeepNameHelper::default())
    }
}

fn prepend_keep_name_helper(code: &mut Vec<u8>, helper: &KeepNameHelper, minify_whitespace: bool) {
    if helper.name.is_empty() {
        return;
    }
    let KeepNameHelper {
        def_prop,
        name,
        target,
        value,
    } = helper;
    let value_property = if value == "value" {
        "value".into()
    } else {
        format!("value: {value}")
    };
    let helper = if minify_whitespace {
        let value_property = value_property.replace(' ', "");
        format!(
            "var {def_prop}=Object.defineProperty;var {name}=({target},{value})=>{def_prop}({target},\"name\",{{{value_property},configurable:true}});"
        )
    } else {
        format!(
            "var {def_prop} = Object.defineProperty;\nvar {name} = ({target}, {value}) => {def_prop}({target}, \"name\", {{ {value_property}, configurable: true }});\n"
        )
    };
    let insertion = if code.starts_with(b"#!") {
        code.iter()
            .position(|byte| *byte == b'\n')
            .map_or(code.len(), |index| index + 1)
    } else {
        0
    };
    code.splice(insertion..insertion, helper.bytes());
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u16)]
pub enum Loader {
    #[default]
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
    Jsx,
    LocalCss,
    Text,
    Ts,
    Tsx,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct TransformOptions {
    pub sourcefile: String,
    pub loader: Loader,
    pub platform: BuildPlatform,
    pub jsx: BuildJsx,
    pub jsx_factory: String,
    pub jsx_fragment: String,
    pub jsx_import_source: String,
    pub jsx_development: bool,
    pub jsx_side_effects: bool,
    pub define: HashMap<String, String>,
    pub pure: Vec<String>,
    pub keep_names: bool,
    pub banner: String,
    pub footer: String,
    pub line_limit: usize,
    pub minify_whitespace: bool,
    pub minify_identifiers: bool,
    pub minify_syntax: bool,
    pub ascii_only: bool,
    pub drop_console: bool,
    pub drop_debugger: bool,
    pub drop_labels: Vec<String>,
    pub ignore_annotations: bool,
    pub legal_comments: BuildLegalComments,
    pub sourcemap: BuildSourceMap,
    pub source_root: String,
    pub sources_content: BuildSourcesContent,
    pub tsconfig_raw: String,
}

impl Default for TransformOptions {
    fn default() -> Self {
        Self {
            sourcefile: String::new(),
            loader: Loader::default(),
            platform: BuildPlatform::default(),
            jsx: BuildJsx::default(),
            jsx_factory: String::new(),
            jsx_fragment: String::new(),
            jsx_import_source: String::new(),
            jsx_development: false,
            jsx_side_effects: false,
            define: HashMap::new(),
            pure: Vec::new(),
            keep_names: false,
            banner: String::new(),
            footer: String::new(),
            line_limit: 0,
            minify_whitespace: false,
            minify_identifiers: false,
            minify_syntax: false,
            ascii_only: true,
            drop_console: false,
            drop_debugger: false,
            drop_labels: Vec::new(),
            ignore_annotations: false,
            legal_comments: BuildLegalComments::default(),
            sourcemap: BuildSourceMap::default(),
            source_root: String::new(),
            sources_content: BuildSourcesContent::default(),
            tsconfig_raw: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MessageKind {
    #[default]
    Error,
    Warning,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Location {
    pub file: String,
    pub namespace: String,
    pub line: usize,
    pub column: usize,
    pub length: usize,
    pub line_text: String,
    pub suggestion: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Note {
    pub text: String,
    pub location: Option<Location>,
}

#[derive(Clone, Default)]
pub struct Message {
    pub id: String,
    pub plugin_name: String,
    pub text: String,
    pub location: Option<Location>,
    pub notes: Vec<Note>,
    pub detail: Option<Arc<dyn Any + Send + Sync>>,
    pub kind: MessageKind,
}

impl fmt::Debug for Message {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Message")
            .field("id", &self.id)
            .field("plugin_name", &self.plugin_name)
            .field("text", &self.text)
            .field("location", &self.location)
            .field("notes", &self.notes)
            .field("detail", &self.detail.as_ref().map(|_| "<opaque>"))
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FormatMessagesOptions {
    pub terminal_width: usize,
    pub kind: MessageKind,
    pub color: bool,
}

#[must_use]
pub fn format_messages(messages: Vec<Message>, options: FormatMessagesOptions) -> Vec<String> {
    let kind = match options.kind {
        MessageKind::Error => MsgKind::Error,
        MessageKind::Warning => MsgKind::Warning,
    };
    messages
        .into_iter()
        .map(|message| {
            Msg {
                notes: message
                    .notes
                    .into_iter()
                    .map(|note| MsgData {
                        text: note.text,
                        location: note.location.map(internal_location),
                        ..MsgData::default()
                    })
                    .collect(),
                plugin_name: message.plugin_name,
                data: MsgData {
                    user_detail: message.detail,
                    location: message.location.map(internal_location),
                    text: message.text,
                    ..MsgData::default()
                },
                kind,
                id: string_to_maximum_msg_id(&message.id),
            }
            .to_string_lossy(
                &OutputOptions {
                    include_source: true,
                    ..OutputOptions::default()
                },
                TerminalInfo {
                    use_color_escapes: options.color,
                    width: options.terminal_width,
                    ..TerminalInfo::default()
                },
            )
        })
        .collect()
}

fn internal_location(location: Location) -> MsgLocation {
    MsgLocation {
        file: PrettyPaths {
            abs: location.file.clone(),
            rel: location.file,
        },
        namespace: if location.namespace.is_empty() {
            "file".into()
        } else {
            location.namespace
        },
        line_text: location.line_text.into_bytes(),
        suggestion: location.suggestion,
        line: location.line,
        column: location.column,
        length: location.length,
    }
}

#[derive(Clone, Debug, Default)]
pub struct TransformResult {
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
    pub code: Vec<u8>,
    pub map: Vec<u8>,
    pub legal_comments: Vec<u8>,
}

#[derive(Default)]
struct TransformPrint {
    code: Vec<u8>,
    extracted_legal_comments: Vec<String>,
    source_map_chunk: SourceMapChunk,
    source_map_prefix_lines: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildFormat {
    #[default]
    Default,
    Iife,
    CommonJs,
    EsModule,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildPlatform {
    #[default]
    Default,
    Browser,
    Node,
    Neutral,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildSourceMap {
    #[default]
    None,
    Linked,
    External,
    Inline,
    InlineAndExternal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildSourcesContent {
    #[default]
    Include,
    Exclude,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildLegalComments {
    #[default]
    Default,
    Inline,
    None,
    EndOfFile,
    Linked,
    External,
}

const fn internal_legal_comments(value: BuildLegalComments, bundle: bool) -> config::LegalComments {
    match value {
        BuildLegalComments::Default => {
            if bundle {
                config::LegalComments::EndOfFile
            } else {
                config::LegalComments::Inline
            }
        }
        BuildLegalComments::Inline => config::LegalComments::Inline,
        BuildLegalComments::None => config::LegalComments::None,
        BuildLegalComments::EndOfFile => config::LegalComments::EndOfFile,
        BuildLegalComments::Linked => config::LegalComments::LinkedWithComment,
        BuildLegalComments::External => config::LegalComments::ExternalWithoutComment,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Packages {
    #[default]
    Bundle,
    External,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildTreeShaking {
    #[default]
    Default,
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildJsx {
    #[default]
    Transform,
    Preserve,
    Automatic,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct BuildOptions {
    pub bundle: bool,
    pub entry_points: Vec<String>,
    pub entry_points_advanced: Vec<BuildEntryPoint>,
    pub stdin: Option<BuildStdin>,
    pub outdir: String,
    pub outfile: String,
    pub outbase: String,
    pub abs_working_dir: String,
    pub tsconfig: String,
    pub tsconfig_raw: String,
    pub metafile: bool,
    pub format: BuildFormat,
    pub platform: BuildPlatform,
    pub global_name: String,
    pub public_path: String,
    pub entry_names: String,
    pub chunk_names: String,
    pub asset_names: String,
    pub sourcemap: BuildSourceMap,
    pub source_root: String,
    pub sources_content: BuildSourcesContent,
    pub legal_comments: BuildLegalComments,
    pub line_limit: usize,
    pub tree_shaking: BuildTreeShaking,
    pub jsx: BuildJsx,
    pub jsx_factory: String,
    pub jsx_fragment: String,
    pub jsx_import_source: String,
    pub jsx_development: bool,
    pub jsx_side_effects: bool,
    pub splitting: bool,
    pub preserve_symlinks: bool,
    pub allow_overwrite: bool,
    pub write: bool,
    pub minify_whitespace: bool,
    pub minify_identifiers: bool,
    pub minify_syntax: bool,
    pub ascii_only: bool,
    pub drop_console: bool,
    pub drop_debugger: bool,
    pub drop_labels: Vec<String>,
    pub ignore_annotations: bool,
    pub banner: String,
    pub footer: String,
    pub css_banner: String,
    pub css_footer: String,
    pub external: Vec<String>,
    pub alias: HashMap<String, String>,
    pub packages: Packages,
    pub loader: HashMap<String, Loader>,
    pub out_extension: HashMap<String, String>,
    pub define: HashMap<String, String>,
    pub pure: Vec<String>,
    pub keep_names: bool,
    pub main_fields: Vec<String>,
    pub resolve_extensions: Vec<String>,
    pub conditions: Vec<String>,
    pub node_paths: Vec<String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            bundle: false,
            entry_points: Vec::new(),
            entry_points_advanced: Vec::new(),
            stdin: None,
            outdir: String::new(),
            outfile: String::new(),
            outbase: String::new(),
            abs_working_dir: String::new(),
            tsconfig: String::new(),
            tsconfig_raw: String::new(),
            metafile: false,
            format: BuildFormat::default(),
            platform: BuildPlatform::default(),
            global_name: String::new(),
            public_path: String::new(),
            entry_names: String::new(),
            chunk_names: String::new(),
            asset_names: String::new(),
            sourcemap: BuildSourceMap::default(),
            source_root: String::new(),
            sources_content: BuildSourcesContent::default(),
            legal_comments: BuildLegalComments::default(),
            line_limit: 0,
            tree_shaking: BuildTreeShaking::default(),
            jsx: BuildJsx::default(),
            jsx_factory: String::new(),
            jsx_fragment: String::new(),
            jsx_import_source: String::new(),
            jsx_development: false,
            jsx_side_effects: false,
            splitting: false,
            preserve_symlinks: false,
            allow_overwrite: false,
            write: false,
            minify_whitespace: false,
            minify_identifiers: false,
            minify_syntax: false,
            ascii_only: true,
            drop_console: false,
            drop_debugger: false,
            drop_labels: Vec::new(),
            ignore_annotations: false,
            banner: String::new(),
            footer: String::new(),
            css_banner: String::new(),
            css_footer: String::new(),
            external: Vec::new(),
            alias: HashMap::new(),
            packages: Packages::default(),
            loader: HashMap::new(),
            out_extension: HashMap::new(),
            define: HashMap::new(),
            pure: Vec::new(),
            keep_names: false,
            main_fields: Vec::new(),
            resolve_extensions: Vec::new(),
            conditions: Vec::new(),
            node_paths: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuildEntryPoint {
    pub input_path: String,
    pub output_path: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuildStdin {
    pub contents: String,
    pub resolve_dir: String,
    pub sourcefile: String,
    pub loader: Loader,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuildOutputFile {
    pub path: String,
    pub contents: Vec<u8>,
    pub hash: String,
    pub executable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BuildResult {
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
    pub metafile: String,
    pub output_files: Vec<BuildOutputFile>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnalyzeMetafileOptions {
    pub color: bool,
    pub verbose: bool,
}

#[derive(Clone, Debug, Default)]
struct MetafileEntry {
    name: String,
    entry_point: String,
    entries: Vec<MetafileEntry>,
    size: usize,
}

#[derive(Clone, Debug, Default)]
struct MetafileTableEntry {
    first: String,
    second: String,
    third: String,
    first_len: usize,
    second_len: usize,
    third_len: usize,
    is_top_level: bool,
}

#[allow(clippy::cast_precision_loss)]
fn pretty_print_byte_count(size: usize) -> String {
    if size < 1024 {
        format!("{size}b ")
    } else if size < 1024 * 1024 {
        format!("{:.1}kb", size as f64 / 1024.0)
    } else if size < 1024 * 1024 * 1024 {
        format!("{:.1}mb", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}gb", size as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[must_use]
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]
pub fn analyze_metafile(metafile: &str, options: AnalyzeMetafileOptions) -> String {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(metafile) else {
        return String::new();
    };
    let Some(outputs) = root.get("outputs").and_then(serde_json::Value::as_object) else {
        return String::new();
    };
    let mut entries = Vec::new();
    let mut entry_points = Vec::new();
    for (name, output) in outputs {
        if name.ends_with(".map") {
            continue;
        }
        let Some(size) = output.get("bytes").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Some(inputs) = output.get("inputs").and_then(serde_json::Value::as_object) else {
            continue;
        };
        let entry_point = output
            .get("entryPoint")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !entry_point.is_empty() {
            entry_points.push(entry_point.clone());
        }
        let mut children = inputs
            .iter()
            .filter_map(|(name, input)| {
                let size = input
                    .get("bytesInOutput")
                    .and_then(serde_json::Value::as_u64)?;
                (size > 0).then(|| MetafileEntry {
                    name: name.clone(),
                    size: usize::try_from(size).unwrap_or(usize::MAX),
                    ..MetafileEntry::default()
                })
            })
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            right
                .size
                .cmp(&left.size)
                .then_with(|| left.name.cmp(&right.name))
        });
        entries.push(MetafileEntry {
            name: name.clone(),
            entry_point,
            entries: children,
            size: usize::try_from(size).unwrap_or(usize::MAX),
        });
    }
    entries.sort_by(|left, right| {
        right
            .size
            .cmp(&left.size)
            .then_with(|| left.name.cmp(&right.name))
    });

    let imports = root
        .get("inputs")
        .and_then(serde_json::Value::as_object)
        .map(|inputs| {
            inputs
                .iter()
                .map(|(name, input)| {
                    let paths = input
                        .get("imports")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|item| item.get("path").and_then(serde_json::Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    (name.clone(), paths)
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let graph_for = |roots: &[String]| {
        let mut graph = roots
            .iter()
            .cloned()
            .map(|path| (path, (String::new(), 0_u32)))
            .collect::<HashMap<_, _>>();
        let mut worklist = roots.to_vec();
        while let Some(path) = worklist.pop() {
            let depth = graph.get(&path).map_or(1, |entry| entry.1 + 1);
            for imported in imports.get(&path).into_iter().flatten() {
                let old_depth = graph.get(imported).map_or(u32::MAX, |entry| entry.1);
                if old_depth > depth {
                    graph.insert(imported.clone(), (path.clone(), depth));
                    worklist.push(imported.clone());
                }
            }
        }
        graph
    };
    let all_graph = options.verbose.then(|| graph_for(&entry_points));
    let mut table = Vec::new();
    for entry in entries {
        let second = pretty_print_byte_count(entry.size);
        table.push(MetafileTableEntry {
            first_len: entry.name.chars().count(),
            second_len: second.len(),
            third_len: 6,
            first: entry.name,
            second,
            third: "100.0%".into(),
            is_top_level: true,
        });
        let entry_graph = (!entry.entry_point.is_empty())
            .then(|| graph_for(std::slice::from_ref(&entry.entry_point)));
        let graph = entry_graph.as_ref().or(all_graph.as_ref());
        let child_count = entry.entries.len();
        for (index, child) in entry.entries.into_iter().enumerate() {
            let last = index + 1 == child_count;
            let first = format!(" {} {}", if last { '└' } else { '├' }, child.name);
            let second = pretty_print_byte_count(child.size);
            let third = format!(
                "{:.1}%",
                100.0 * child.size as f64 / entry.size.max(1) as f64
            );
            table.push(MetafileTableEntry {
                first_len: first.chars().count(),
                second_len: second.len(),
                third_len: third.len(),
                first,
                second,
                third,
                ..MetafileTableEntry::default()
            });
            if options.verbose {
                let indent = if last { "   " } else { " │ " };
                let mut current = graph
                    .and_then(|graph| graph.get(&child.name))
                    .cloned()
                    .unwrap_or_default();
                let mut depth = 0;
                while current.1 != 0 {
                    let first = format!("{indent}{} └ {}", " ".repeat(depth), current.0);
                    table.push(MetafileTableEntry {
                        first,
                        ..MetafileTableEntry::default()
                    });
                    current = graph
                        .and_then(|graph| graph.get(&current.0))
                        .cloned()
                        .unwrap_or_default();
                    depth += 3;
                }
            }
        }
    }
    render_metafile_table(&table, options)
}

fn render_metafile_table(table: &[MetafileTableEntry], options: AnalyzeMetafileOptions) -> String {
    let max_first = table.iter().map(|entry| entry.first_len).max().unwrap_or(0);
    let max_second = table
        .iter()
        .map(|entry| entry.second_len)
        .max()
        .unwrap_or(0);
    let max_third = table.iter().map(|entry| entry.third_len).max().unwrap_or(0);
    let colors = if options.color {
        crate::internal::logger::TERMINAL_COLORS
    } else {
        crate::internal::logger::Colors::default()
    };
    let mut result = String::new();
    for entry in table {
        if entry.is_top_level {
            result.push('\n');
        }
        if entry.second.is_empty() && entry.third.is_empty() {
            result.push_str("  ");
            result.push_str(&entry.first);
            result.push('\n');
            continue;
        }
        let trimmed = entry.second.trim_end();
        let line = if options.verbose { '─' } else { ' ' };
        let extra = usize::from(options.verbose);
        let color = if entry.is_top_level { colors.bold } else { "" };
        result.push_str("  ");
        result.push_str(color);
        result.push_str(&entry.first);
        result.push_str(colors.reset);
        result.push(' ');
        result.push_str(colors.dim);
        result.extend(std::iter::repeat_n(
            line,
            extra + max_first - entry.first_len + max_second - entry.second_len,
        ));
        result.push_str(colors.reset);
        result.push(' ');
        result.push_str(color);
        result.push_str(trimmed);
        result.push_str(colors.reset);
        result.push(' ');
        result.push_str(colors.dim);
        result.extend(std::iter::repeat_n(
            line,
            extra + max_third - entry.third_len + entry.second.len() - trimmed.len(),
        ));
        result.push_str(colors.reset);
        result.push(' ');
        result.push_str(color);
        result.push_str(&entry.third);
        result.push_str(colors.reset);
        result.push('\n');
    }
    result
}

fn output_file_hash(contents: &[u8]) -> String {
    STANDARD_NO_PAD.encode(xxhash::sum64(contents).to_le_bytes())
}

fn parse_tsconfig_raw(
    log: &Log,
    file_system: &dyn Fs,
    directory: &str,
    contents: &str,
) -> Option<resolver::TsConfigJson> {
    if contents.is_empty() {
        return None;
    }
    resolver::parse_tsconfig_json(
        log,
        &Source {
            key_path: Path {
                text: "<tsconfig.json>".into(),
                ..Path::default()
            },
            pretty_paths: PrettyPaths {
                abs: "<tsconfig.json>".into(),
                rel: "<tsconfig.json>".into(),
            },
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        },
        file_system,
        directory,
        directory,
        None,
    )
}

fn write_build_output_files(
    output_files: &[BuildOutputFile],
    write_to_stdout: bool,
    input_paths: &HashSet<PathBuf>,
    allow_overwrite: bool,
) -> Vec<Message> {
    if write_to_stdout {
        if output_files.len() != 1 {
            return vec![Message {
                text: format!(
                    "Internal error: did not expect to generate {} files when writing to stdout",
                    output_files.len()
                ),
                kind: MessageKind::Error,
                ..Message::default()
            }];
        }
        return io::stdout()
            .write_all(&output_files[0].contents)
            .err()
            .map_or_else(Vec::new, |error| {
                vec![Message {
                    text: format!("Could not write to stdout: {error}"),
                    kind: MessageKind::Error,
                    ..Message::default()
                }]
            });
    }

    let mut errors = Vec::new();
    for output in output_files {
        let path = FsPath::new(&output.path);
        if !allow_overwrite
            && std_fs::canonicalize(path).is_ok_and(|canonical| input_paths.contains(&canonical))
        {
            errors.push(Message {
                text: format!(
                    "Refusing to overwrite input file {} (use allow_overwrite to allow this)",
                    path.display()
                ),
                kind: MessageKind::Error,
                ..Message::default()
            });
            continue;
        }
        if let Some(parent) = path.parent()
            && let Err(error) = std_fs::create_dir_all(parent)
        {
            errors.push(Message {
                text: format!(
                    "Could not create output directory {}: {error}",
                    parent.display()
                ),
                kind: MessageKind::Error,
                ..Message::default()
            });
            continue;
        }
        if let Err(error) = std_fs::write(path, &output.contents) {
            errors.push(Message {
                text: format!("Could not write output file {}: {error}", path.display()),
                kind: MessageKind::Error,
                ..Message::default()
            });
            continue;
        }
        #[cfg(unix)]
        if output.executable {
            use std::os::unix::fs::PermissionsExt;

            let permissions = std_fs::metadata(path).map(|metadata| metadata.permissions());
            if let Ok(mut permissions) = permissions {
                permissions.set_mode(permissions.mode() | 0o111);
                if let Err(error) = std_fs::set_permissions(path, permissions) {
                    errors.push(Message {
                        text: format!(
                            "Could not make output file {} executable: {error}",
                            path.display()
                        ),
                        kind: MessageKind::Error,
                        ..Message::default()
                    });
                }
            }
        }
    }
    errors
}

fn validate_externals(
    file_system: &dyn Fs,
    paths: &[String],
) -> Result<config::ExternalSettings, Vec<Message>> {
    let mut result = config::ExternalSettings::default();
    let mut errors = Vec::new();
    for path in paths {
        if let Some(index) = path.find('*') {
            if path[index + 1..].contains('*') {
                errors.push(Message {
                    text: format!(
                        "External path {path:?} cannot have more than one \"*\" wildcard"
                    ),
                    kind: MessageKind::Error,
                    ..Message::default()
                });
                continue;
            }
            result.pre_resolve.patterns.push(config::WildcardPattern {
                prefix: path[..index].into(),
                suffix: path[index + 1..].into(),
            });
            if !resolver::is_package_path(path) {
                let absolute = if file_system.is_abs(path) {
                    path.clone()
                } else {
                    file_system.join(&[file_system.cwd(), path])
                };
                let absolute_index = absolute.find('*').expect("wildcard is preserved");
                result.post_resolve.patterns.push(config::WildcardPattern {
                    prefix: absolute[..absolute_index].into(),
                    suffix: absolute[absolute_index + 1..].into(),
                });
            }
        } else {
            result.pre_resolve.exact.insert(path.clone(), true);
            if resolver::is_package_path(path) {
                result.pre_resolve.patterns.push(config::WildcardPattern {
                    prefix: format!("{path}/"),
                    suffix: String::new(),
                });
            } else {
                let absolute = if file_system.is_abs(path) {
                    path.clone()
                } else {
                    file_system.join(&[file_system.cwd(), path])
                };
                result.post_resolve.exact.insert(absolute, true);
            }
        }
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}

fn default_abs_output_base(
    file_system: &dyn Fs,
    entry_points: &[String],
    preserve_symlinks: bool,
) -> String {
    let mut directories = entry_points.iter().map(|entry_point| {
        let mut absolute = if file_system.is_abs(entry_point) {
            entry_point.clone()
        } else {
            file_system.join(&[file_system.cwd(), entry_point])
        };
        if !preserve_symlinks && let Some(real_path) = file_system.eval_symlinks(&absolute) {
            absolute = real_path;
        }
        PathBuf::from(file_system.dir(&absolute))
    });
    let Some(mut common) = directories.next() else {
        return file_system.cwd().to_string();
    };
    for directory in directories {
        while !directory.starts_with(&common) {
            if !common.pop() {
                return file_system.cwd().to_string();
            }
        }
    }
    common.to_string_lossy().into_owned()
}

const fn build_loader(loader: Loader) -> config::Loader {
    match loader {
        Loader::None => config::Loader::None,
        Loader::Base64 => config::Loader::Base64,
        Loader::Binary => config::Loader::Binary,
        Loader::Copy => config::Loader::Copy,
        Loader::Css => config::Loader::Css,
        Loader::DataUrl => config::Loader::DataUrl,
        Loader::Default => config::Loader::Default,
        Loader::Empty => config::Loader::Empty,
        Loader::File => config::Loader::File,
        Loader::GlobalCss => config::Loader::GlobalCss,
        Loader::Js => config::Loader::Js,
        Loader::Json => config::Loader::Json,
        Loader::Jsx => config::Loader::Jsx,
        Loader::LocalCss => config::Loader::LocalCss,
        Loader::Text => config::Loader::Text,
        Loader::Ts => config::Loader::Ts,
        Loader::Tsx => config::Loader::Tsx,
    }
}

fn validate_build_loaders(
    loaders: &HashMap<String, Loader>,
) -> Result<HashMap<String, config::Loader>, Vec<Message>> {
    let mut result = bundler::default_extension_to_loader_map();
    let mut errors = Vec::new();
    for (extension, &loader) in loaders {
        if !extension.is_empty()
            && (extension.len() < 2 || !extension.starts_with('.') || extension.ends_with('.'))
        {
            errors.push(Message {
                text: format!("Invalid file extension: {extension:?}"),
                kind: MessageKind::Error,
                ..Message::default()
            });
        }
        result.insert(extension.clone(), build_loader(loader));
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}

fn validate_output_extensions(
    extensions: &HashMap<String, String>,
) -> Result<(String, String), Vec<Message>> {
    let mut javascript = String::new();
    let mut css = String::new();
    let mut errors = Vec::new();
    for (kind, extension) in extensions {
        let target = match kind.as_str() {
            ".js" => &mut javascript,
            ".css" => &mut css,
            _ => {
                errors.push(Message {
                    text: format!("Invalid output extension key: {kind:?}"),
                    kind: MessageKind::Error,
                    ..Message::default()
                });
                continue;
            }
        };
        if extension.len() < 2 || !extension.starts_with('.') || extension.ends_with('.') {
            errors.push(Message {
                text: format!("Invalid output extension: {extension:?}"),
                kind: MessageKind::Error,
                ..Message::default()
            });
        } else {
            target.clone_from(extension);
        }
    }
    if errors.is_empty() {
        Ok((javascript, css))
    } else {
        Err(errors)
    }
}

fn validate_resolve_extensions(extensions: &[String]) -> Result<(), Vec<Message>> {
    let errors = extensions
        .iter()
        .filter(|extension| {
            extension.len() < 2 || !extension.starts_with('.') || extension.ends_with('.')
        })
        .map(|extension| Message {
            text: format!("Invalid file extension: {extension:?}"),
            kind: MessageKind::Error,
            ..Message::default()
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_path_template(template: &str) -> Vec<config::PathTemplate> {
    if template.is_empty() {
        return Vec::new();
    }
    let mut template = format!("./{}", template.replace('\\', "/"));
    let mut parts = Vec::new();
    let mut search = 0;
    while search < template.len() {
        let Some(found) = template[search..].find('[') else {
            break;
        };
        search += found;
        let tail = &template[search..];
        let (placeholder, length) = if tail.starts_with("[dir]") {
            (config::PathPlaceholder::Dir, "[dir]".len())
        } else if tail.starts_with("[name]") {
            (config::PathPlaceholder::Name, "[name]".len())
        } else if tail.starts_with("[hash]") {
            (config::PathPlaceholder::Hash, "[hash]".len())
        } else if tail.starts_with("[ext]") {
            (config::PathPlaceholder::Ext, "[ext]".len())
        } else {
            search += 1;
            continue;
        };
        parts.push(config::PathTemplate {
            data: template[..search].into(),
            placeholder,
        });
        template = template[search + length..].into();
        search = 0;
    }
    if search < template.len() {
        parts.push(config::PathTemplate {
            data: template,
            ..config::PathTemplate::default()
        });
    }
    parts
}

fn validate_jsx_define(
    log: &Log,
    text: &str,
    option_name: &str,
    allow_constant: bool,
) -> config::DefineExpr {
    if text.is_empty() {
        return config::DefineExpr::default();
    }
    let (define, injected) = js_parser::parse_define_expr(text);
    if !define.parts.is_empty() || (allow_constant && define.constant.data.is_some()) {
        return define;
    }
    let _ = injected;
    log.add_error(
        None,
        crate::internal::logger::Range::default(),
        format!("Invalid value for {option_name}: {text:?}"),
    );
    config::DefineExpr::default()
}

#[allow(clippy::too_many_lines)]
fn validate_defines(
    log: &Log,
    defines: &HashMap<String, String>,
    pure: &[String],
    platform: BuildPlatform,
    minify: bool,
) -> Arc<config::ProcessedDefines> {
    let mut keys = defines.keys().collect::<Vec<_>>();
    keys.sort();
    let mut raw = Vec::with_capacity(keys.len() + pure.len());
    let mut injected_defines = Vec::new();
    for value in pure {
        let (key_parts, ok) = js_parser::parse_global_name(
            log.clone(),
            Source {
                contents: Arc::from(value.as_bytes()),
                ..Source::default()
            },
        );
        if ok {
            raw.push(config::DefineData {
                key_parts: key_parts
                    .into_iter()
                    .map(|part| String::from_utf8_lossy(&part).into_owned())
                    .collect(),
                flags: config::DefineFlags::CALL_CAN_BE_UNWRAPPED_IF_UNUSED,
                ..config::DefineData::default()
            });
        }
    }
    for key in keys {
        let (key_parts, ok) = js_parser::parse_global_name(
            log.clone(),
            Source {
                contents: Arc::from(key.as_bytes()),
                ..Source::default()
            },
        );
        if !ok {
            continue;
        }
        let value = &defines[key];
        let (define_expr, injected) = js_parser::parse_define_expr(value);
        let key_parts = key_parts
            .into_iter()
            .map(|part| String::from_utf8_lossy(&part).into_owned())
            .collect::<Vec<_>>();
        if define_expr.constant.data.is_some() || !define_expr.parts.is_empty() {
            raw.push(config::DefineData {
                key_parts,
                define_expr: Some(define_expr),
                ..config::DefineData::default()
            });
        } else if injected.data.is_some() {
            let injected_define_index = crate::internal::ast::Index32::new(
                u32::try_from(injected_defines.len()).expect("injected define count fits in u32"),
            );
            let name = format!(
                "define_{}_default",
                key.chars()
                    .map(|character| {
                        if character.is_ascii_alphanumeric() || character == '_' {
                            character
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>()
            );
            injected_defines.push(config::InjectedDefine {
                data: injected,
                name,
                ..config::InjectedDefine::default()
            });
            raw.push(config::DefineData {
                key_parts,
                define_expr: Some(config::DefineExpr {
                    injected_define_index,
                    ..config::DefineExpr::default()
                }),
                ..config::DefineData::default()
            });
        } else {
            log.add_error(
                None,
                crate::internal::logger::Range::default(),
                format!("Invalid define value (must be an entity name or JS literal): {value}"),
            );
        }
    }
    if matches!(platform, BuildPlatform::Default | BuildPlatform::Browser)
        && !raw.iter().any(|define| {
            let parts = define
                .key_parts
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            matches!(
                parts.as_slice(),
                ["process"] | ["process", "env"] | ["process", "env", "NODE_ENV"]
            )
        })
    {
        let (define_expr, _) = js_parser::parse_define_expr(if minify {
            r#""production""#
        } else {
            r#""development""#
        });
        raw.push(config::DefineData {
            key_parts: vec!["process".into(), "env".into(), "NODE_ENV".into()],
            define_expr: Some(define_expr),
            ..config::DefineData::default()
        });
    }
    let mut processed = config::process_defines(&raw);
    processed.injected_defines = injected_defines;
    Arc::new(processed)
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build(options: BuildOptions) -> BuildResult {
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let bundle = options.bundle;
    let write = options.write;
    let write_to_stdout = write && options.outdir.is_empty() && options.outfile.is_empty();
    let allow_overwrite = options.allow_overwrite;
    let file_system = match real_fs(RealFsOptions {
        abs_working_dir: options.abs_working_dir.clone(),
        ..RealFsOptions::default()
    }) {
        Ok(file_system) => file_system,
        Err(error) => {
            return BuildResult {
                errors: vec![Message {
                    text: error.message,
                    kind: MessageKind::Error,
                    ..Message::default()
                }],
                ..BuildResult::default()
            };
        }
    };
    let entry_point_count = options.entry_points.len()
        + options.entry_points_advanced.len()
        + usize::from(options.stdin.is_some());
    let output_topology_error = if options.outdir.is_empty() && entry_point_count > 1 {
        Some("Must use \"outdir\" when there are multiple input files")
    } else if options.outdir.is_empty() && options.splitting {
        Some("Must use \"outdir\" when code splitting is enabled")
    } else if !options.outfile.is_empty() && !options.outdir.is_empty() {
        Some("Cannot use both \"outfile\" and \"outdir\"")
    } else {
        None
    };
    if let Some(text) = output_topology_error {
        return BuildResult {
            errors: vec![Message {
                text: text.into(),
                kind: MessageKind::Error,
                ..Message::default()
            }],
            ..BuildResult::default()
        };
    }
    if options.outdir.is_empty() && options.outfile.is_empty() {
        let mut errors = Vec::new();
        if !matches!(
            options.sourcemap,
            BuildSourceMap::None | BuildSourceMap::Inline
        ) {
            errors.push(Message {
                text: "Cannot use an external source map without an output path".into(),
                kind: MessageKind::Error,
                ..Message::default()
            });
        }
        if matches!(
            options.legal_comments,
            BuildLegalComments::Linked | BuildLegalComments::External
        ) {
            errors.push(Message {
                text: "Cannot use linked or external legal comments without an output path".into(),
                kind: MessageKind::Error,
                ..Message::default()
            });
        }
        if options
            .loader
            .values()
            .any(|loader| *loader == Loader::File)
        {
            errors.push(Message {
                text: "Cannot use the \"file\" loader without an output path".into(),
                kind: MessageKind::Error,
                ..Message::default()
            });
        }
        if options
            .loader
            .values()
            .any(|loader| *loader == Loader::Copy)
        {
            errors.push(Message {
                text: "Cannot use the \"copy\" loader without an output path".into(),
                kind: MessageKind::Error,
                ..Message::default()
            });
        }
        if !errors.is_empty() {
            return BuildResult {
                errors,
                ..BuildResult::default()
            };
        }
    }
    if !bundle {
        let mut errors = Vec::new();
        if !options.external.is_empty() {
            errors.push(Message {
                text: "Cannot use \"external\" without \"bundle\"".into(),
                kind: MessageKind::Error,
                ..Message::default()
            });
        }
        if !options.alias.is_empty() {
            errors.push(Message {
                text: "Cannot use \"alias\" without \"bundle\"".into(),
                kind: MessageKind::Error,
                ..Message::default()
            });
        }
        if !errors.is_empty() {
            return BuildResult {
                errors,
                ..BuildResult::default()
            };
        }
    }
    let external_settings = match validate_externals(file_system.as_ref(), &options.external) {
        Ok(settings) => settings,
        Err(errors) => {
            return BuildResult {
                errors,
                ..BuildResult::default()
            };
        }
    };
    let canonical_input_paths = options
        .entry_points
        .iter()
        .chain(
            options
                .entry_points_advanced
                .iter()
                .map(|entry| &entry.input_path),
        )
        .filter_map(|path| {
            let path = if file_system.is_abs(path) {
                path.clone()
            } else {
                file_system.join(&[file_system.cwd(), path])
            };
            std_fs::canonicalize(path).ok()
        })
        .collect::<HashSet<_>>();
    let all_entry_point_paths = options
        .entry_points
        .iter()
        .chain(
            options
                .entry_points_advanced
                .iter()
                .map(|entry| &entry.input_path),
        )
        .cloned()
        .collect::<Vec<_>>();
    let abs_output_base = if options.outbase.is_empty() {
        default_abs_output_base(
            file_system.as_ref(),
            &all_entry_point_paths,
            options.preserve_symlinks,
        )
    } else if file_system.is_abs(&options.outbase) {
        options.outbase.clone()
    } else {
        file_system.join(&[file_system.cwd(), &options.outbase])
    };
    let extension_to_loader = match validate_build_loaders(&options.loader) {
        Ok(loaders) => loaders,
        Err(errors) => {
            return BuildResult {
                errors,
                ..BuildResult::default()
            };
        }
    };
    let (output_extension_js, output_extension_css) =
        match validate_output_extensions(&options.out_extension) {
            Ok(extensions) => extensions,
            Err(errors) => {
                return BuildResult {
                    errors,
                    ..BuildResult::default()
                };
            }
        };
    if let Err(errors) = validate_resolve_extensions(&options.resolve_extensions) {
        return BuildResult {
            errors,
            ..BuildResult::default()
        };
    }
    let global_name = if options.global_name.is_empty() {
        Vec::new()
    } else {
        let (parts, ok) = js_parser::parse_global_name(
            log.clone(),
            Source {
                key_path: crate::internal::logger::Path {
                    text: "<global-name>".into(),
                    ..crate::internal::logger::Path::default()
                },
                pretty_paths: PrettyPaths {
                    abs: "<global-name>".into(),
                    rel: "<global-name>".into(),
                },
                contents: Arc::from(options.global_name.as_bytes()),
                ..Source::default()
            },
        );
        if !ok {
            let (errors, warnings) = public_messages(log.done());
            return BuildResult {
                errors,
                warnings,
                ..BuildResult::default()
            };
        }
        parts
            .into_iter()
            .map(|part| String::from_utf8_lossy(&part).into_owned())
            .collect()
    };
    let defines = validate_defines(
        &log,
        &options.define,
        &options.pure,
        options.platform,
        options.minify_whitespace && options.minify_identifiers && options.minify_syntax,
    );
    let jsx_factory = validate_jsx_define(&log, &options.jsx_factory, "jsx factory", false);
    let jsx_fragment = validate_jsx_define(&log, &options.jsx_fragment, "jsx fragment", true);
    if !options.tsconfig.is_empty() && !options.tsconfig_raw.is_empty() {
        log.add_error(
            None,
            crate::internal::logger::Range::default(),
            "Cannot provide \"tsconfig\" as both a raw string and a path",
        );
    }
    let raw_tsconfig = parse_tsconfig_raw(
        &log,
        file_system.as_ref(),
        file_system.cwd(),
        &options.tsconfig_raw,
    );
    let mut jsx_options = config::JsxOptions {
        factory: jsx_factory,
        fragment: jsx_fragment,
        preserve: options.jsx == BuildJsx::Preserve,
        automatic_runtime: options.jsx == BuildJsx::Automatic,
        import_source: options.jsx_import_source,
        development: options.jsx_development,
        side_effects: options.jsx_side_effects,
        ..config::JsxOptions::default()
    };
    let mut ts_options = config::TsOptions::default();
    let mut ts_always_strict = None;
    if let Some(tsconfig) = raw_tsconfig {
        tsconfig.jsx_settings.apply_to(&mut jsx_options);
        ts_options.config = tsconfig.settings;
        ts_always_strict = tsconfig.ts_always_strict_or_strict().cloned().map(Arc::new);
    }
    if log.has_errors() {
        let (errors, warnings) = public_messages(log.done());
        return BuildResult {
            errors,
            warnings,
            ..BuildResult::default()
        };
    }
    let output_file = if options.outfile.is_empty() {
        String::new()
    } else if file_system.is_abs(&options.outfile) {
        options.outfile.clone()
    } else {
        file_system.join(&[file_system.cwd(), &options.outfile])
    };
    let output_dir = if !options.outdir.is_empty() {
        if file_system.is_abs(&options.outdir) {
            options.outdir.clone()
        } else {
            file_system.join(&[file_system.cwd(), &options.outdir])
        }
    } else if !output_file.is_empty() {
        file_system.dir(&output_file)
    } else {
        file_system.cwd().to_string()
    };
    let tsconfig_path = if options.tsconfig.is_empty() {
        String::new()
    } else if file_system.is_abs(&options.tsconfig) {
        options.tsconfig.clone()
    } else {
        file_system.join(&[file_system.cwd(), &options.tsconfig])
    };
    let stdin = options.stdin.map(|stdin| config::StdinInfo {
        contents: stdin.contents,
        source_file: stdin.sourcefile,
        abs_resolve_dir: if stdin.resolve_dir.is_empty() {
            String::new()
        } else if file_system.is_abs(&stdin.resolve_dir) {
            stdin.resolve_dir
        } else {
            file_system.join(&[file_system.cwd(), &stdin.resolve_dir])
        },
        loader: build_loader(stdin.loader),
    });
    let abs_node_paths = options
        .node_paths
        .iter()
        .map(|path| {
            if file_system.is_abs(path) {
                path.clone()
            } else {
                file_system.join(&[file_system.cwd(), path])
            }
        })
        .collect();
    let output_format = match options.format {
        BuildFormat::Default if bundle => match options.platform {
            BuildPlatform::Default | BuildPlatform::Browser => config::Format::Iife,
            BuildPlatform::Node => config::Format::CommonJs,
            BuildPlatform::Neutral => config::Format::EsModule,
        },
        BuildFormat::Default => config::Format::Preserve,
        BuildFormat::Iife => config::Format::Iife,
        BuildFormat::CommonJs => config::Format::CommonJs,
        BuildFormat::EsModule => config::Format::EsModule,
    };
    let mode = if bundle {
        Mode::Bundle
    } else if output_format == config::Format::Preserve {
        Mode::PassThrough
    } else {
        Mode::ConvertFormat
    };
    if options.splitting && output_format != config::Format::EsModule {
        return BuildResult {
            errors: vec![Message {
                text: "Splitting currently only works with the \"esm\" format".into(),
                kind: MessageKind::Error,
                ..Message::default()
            }],
            ..BuildResult::default()
        };
    }
    let mut internal_options = config::Options {
        mode,
        output_format,
        platform: match options.platform {
            BuildPlatform::Default | BuildPlatform::Browser => config::Platform::Browser,
            BuildPlatform::Node => config::Platform::Node,
            BuildPlatform::Neutral => config::Platform::Neutral,
        },
        source_map: match options.sourcemap {
            BuildSourceMap::None => config::SourceMap::None,
            BuildSourceMap::Linked => config::SourceMap::LinkedWithComment,
            BuildSourceMap::External => config::SourceMap::ExternalWithoutComment,
            BuildSourceMap::Inline => config::SourceMap::Inline,
            BuildSourceMap::InlineAndExternal => config::SourceMap::InlineAndExternal,
        },
        source_root: options.source_root,
        exclude_sources_content: options.sources_content == BuildSourcesContent::Exclude,
        legal_comments: internal_legal_comments(options.legal_comments, bundle),
        line_limit: options.line_limit,
        code_splitting: options.splitting,
        preserve_symlinks: options.preserve_symlinks,
        allow_overwrite: options.allow_overwrite,
        tree_shaking: match options.tree_shaking {
            BuildTreeShaking::Default => bundle || output_format == config::Format::Iife,
            BuildTreeShaking::Enabled => true,
            BuildTreeShaking::Disabled => false,
        },
        jsx: jsx_options,
        ts: ts_options,
        ts_always_strict,
        minify_whitespace: options.minify_whitespace,
        minify_identifiers: options.minify_identifiers,
        minify_syntax: options.minify_syntax,
        ascii_only: options.ascii_only,
        drop_console: options.drop_console,
        drop_debugger: options.drop_debugger,
        drop_labels: options.drop_labels,
        ignore_dce_annotations: options.ignore_annotations,
        keep_names: options.keep_names,
        js_banner: options.banner,
        js_footer: options.footer,
        css_banner: options.css_banner,
        css_footer: options.css_footer,
        external_settings,
        external_packages: options.packages == Packages::External,
        package_aliases: options.alias,
        extension_to_loader,
        output_extension_js,
        output_extension_css,
        extension_order: options.resolve_extensions,
        main_fields: options.main_fields,
        conditions: options.conditions,
        abs_node_paths,
        global_name,
        public_path: options.public_path,
        entry_path_template: validate_path_template(&options.entry_names),
        chunk_path_template: validate_path_template(&options.chunk_names),
        asset_path_template: validate_path_template(&options.asset_names),
        defines: Some(defines),
        abs_output_dir: output_dir,
        abs_output_file: output_file,
        abs_output_base,
        tsconfig_path,
        tsconfig_raw: options.tsconfig_raw,
        stdin,
        needs_metafile: options.metafile,
        ..config::Options::default()
    };
    let mut entry_points: Vec<_> = options
        .entry_points
        .into_iter()
        .map(|input_path| bundler::EntryPoint {
            input_path,
            input_path_in_file_namespace: true,
            ..bundler::EntryPoint::default()
        })
        .collect();
    entry_points.extend(options.entry_points_advanced.into_iter().map(|entry| {
        bundler::EntryPoint {
            input_path: entry.input_path,
            output_path: entry.output_path,
            input_path_in_file_namespace: true,
        }
    }));
    let compiled = bundler::bundle_javascript(
        &log,
        file_system.as_ref(),
        &CacheSet::default(),
        &entry_points,
        &mut internal_options,
        "API",
    );
    let (mut errors, warnings) = public_messages(log.done());
    if errors.is_empty() {
        errors.extend(
            compiled
                .scan_result
                .import_issues
                .iter()
                .map(|(_, issue)| Message {
                    text: format!("Could not resolve imported symbol {:?}", issue.result.alias),
                    kind: MessageKind::Error,
                    ..Message::default()
                }),
        );
    }
    let metafile = if errors.is_empty() {
        compiled.metafile
    } else {
        String::new()
    };
    let output_files: Vec<BuildOutputFile> = if errors.is_empty() {
        compiled
            .output_files
            .into_iter()
            .map(|output| BuildOutputFile {
                path: output.abs_path,
                hash: output_file_hash(&output.contents),
                contents: output.contents,
                executable: output.is_executable,
            })
            .collect()
    } else {
        Vec::new()
    };
    if write && errors.is_empty() {
        errors.extend(write_build_output_files(
            &output_files,
            write_to_stdout,
            &canonical_input_paths,
            allow_overwrite,
        ));
    }
    BuildResult {
        errors,
        warnings,
        metafile,
        output_files,
    }
}

#[must_use]
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_lines)]
pub fn transform(input: impl AsRef<[u8]>, options: TransformOptions) -> TransformResult {
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let mut options = options;
    if options.legal_comments == BuildLegalComments::Linked {
        return TransformResult {
            errors: vec![Message {
                text: "Cannot transform with linked legal comments".into(),
                kind: MessageKind::Error,
                ..Message::default()
            }],
            ..TransformResult::default()
        };
    }
    if options.sourcemap == BuildSourceMap::Linked {
        return TransformResult {
            errors: vec![Message {
                text: "Cannot transform with linked source maps".into(),
                kind: MessageKind::Error,
                ..Message::default()
            }],
            ..TransformResult::default()
        };
    }
    if options.sourcemap != BuildSourceMap::None && options.sourcefile.is_empty() {
        return TransformResult {
            errors: vec![Message {
                text: "Must use \"sourcefile\" with \"sourcemap\" to set the original file name"
                    .into(),
                kind: MessageKind::Error,
                ..Message::default()
            }],
            ..TransformResult::default()
        };
    }
    let sourcefile = if options.sourcefile.is_empty() {
        "<stdin>".to_string()
    } else {
        options.sourcefile.clone()
    };
    if options.loader == Loader::Default {
        let Some(loader) = default_loader_for_sourcefile(&sourcefile) else {
            return TransformResult {
                errors: vec![Message {
                    text: format!("Do not know how to load path: {sourcefile}"),
                    kind: MessageKind::Error,
                    ..Message::default()
                }],
                ..TransformResult::default()
            };
        };
        options.loader = loader;
    }
    let input_contents = Arc::<[u8]>::from(input.as_ref());
    let source = Source {
        pretty_paths: PrettyPaths {
            abs: sourcefile.clone(),
            rel: sourcefile.clone(),
        },
        identifier_name: generate_non_unique_name_from_path(&sourcefile),
        contents: input_contents.clone(),
        ..Source::default()
    };

    let mut printed = match options.loader {
        Loader::Css | Loader::GlobalCss | Loader::LocalCss => transform_css(&log, source, &options),
        Loader::Js | Loader::Jsx | Loader::Ts | Loader::Tsx | Loader::None => {
            transform_javascript(&log, source, &options)
        }
        Loader::Json => TransformPrint {
            code: transform_json(&log, source, &options),
            ..TransformPrint::default()
        },
        Loader::Text => TransformPrint {
            code: transform_text(&source, &options),
            ..TransformPrint::default()
        },
        Loader::Base64 => TransformPrint {
            code: transform_base64(&source, &options),
            ..TransformPrint::default()
        },
        Loader::Binary => TransformPrint {
            code: transform_binary(&source, &options),
            ..TransformPrint::default()
        },
        Loader::DataUrl => TransformPrint {
            code: transform_data_url(&source, &options),
            ..TransformPrint::default()
        },
        Loader::Empty => TransformPrint::default(),
        loader => {
            let message = format!("Transform loader {loader:?} is not implemented yet");
            return TransformResult {
                errors: vec![Message {
                    text: message,
                    kind: MessageKind::Error,
                    ..Message::default()
                }],
                ..TransformResult::default()
            };
        }
    };

    let messages = log.done();
    let (errors, warnings) = public_messages(messages);
    let mut legal_comments = Vec::new();
    let mut source_map = Vec::new();
    if errors.is_empty() {
        printed.code = add_banner_and_footer(printed.code, &options.banner, "");
        let slash_tag = if matches!(
            options.loader,
            Loader::Css | Loader::GlobalCss | Loader::LocalCss
        ) {
            "/style"
        } else {
            "/script"
        };
        let rendered = render_legal_comments(&printed.extracted_legal_comments, slash_tag);
        match options.legal_comments {
            BuildLegalComments::EndOfFile => printed.code.extend(rendered),
            BuildLegalComments::External => legal_comments = rendered,
            BuildLegalComments::Default
            | BuildLegalComments::Inline
            | BuildLegalComments::None
            | BuildLegalComments::Linked => {}
        }
        printed.code = add_banner_and_footer(printed.code, "", &options.footer);
        if options.sourcemap != BuildSourceMap::None {
            let mut banner_offset = LineColumnOffset::default();
            if !options.banner.is_empty() {
                banner_offset.advance_bytes(options.banner.as_bytes());
                banner_offset.advance_bytes(b"\n");
            }
            let banner_lines = usize::try_from(banner_offset.lines).unwrap_or_default();
            source_map = generate_transform_source_map(
                &sourcefile,
                &input_contents,
                &printed.source_map_chunk,
                printed.source_map_prefix_lines + banner_lines,
                &options,
            );
            if matches!(
                options.sourcemap,
                BuildSourceMap::Inline | BuildSourceMap::InlineAndExternal
            ) {
                append_inline_source_map(
                    &mut printed.code,
                    &source_map,
                    matches!(
                        options.loader,
                        Loader::Css | Loader::GlobalCss | Loader::LocalCss
                    ),
                );
            }
            if options.sourcemap == BuildSourceMap::Inline {
                source_map.clear();
            }
        }
    } else {
        printed.code.clear();
    }
    TransformResult {
        errors,
        warnings,
        code: printed.code,
        map: source_map,
        legal_comments,
    }
}

fn generate_transform_source_map(
    sourcefile: &str,
    contents: &[u8],
    chunk: &SourceMapChunk,
    generated_prefix_lines: usize,
    options: &TransformOptions,
) -> Vec<u8> {
    let mut result = b"{\n  \"version\": 3,\n  \"sources\": [".to_vec();
    result.extend(quote_for_json(sourcefile.as_bytes(), options.ascii_only));
    result.push(b']');
    if !options.source_root.is_empty() {
        result.extend_from_slice(b",\n  \"sourceRoot\": ");
        result.extend(quote_for_json(
            options.source_root.as_bytes(),
            options.ascii_only,
        ));
    }
    if options.sources_content == BuildSourcesContent::Include {
        result.extend_from_slice(b",\n  \"sourcesContent\": [");
        result.extend(quote_for_json(contents, options.ascii_only));
        result.push(b']');
    }
    result.extend_from_slice(b",\n  \"mappings\": \"");
    result.extend(std::iter::repeat_n(b';', generated_prefix_lines));
    result.extend_from_slice(&chunk.buffer.data);
    result.extend_from_slice(b"\",\n  \"names\": [");
    for (index, name) in chunk.quoted_names.iter().enumerate() {
        if index != 0 {
            result.extend_from_slice(b", ");
        }
        result.extend_from_slice(name);
    }
    result.extend_from_slice(b"]\n}\n");
    result
}

fn append_inline_source_map(code: &mut Vec<u8>, source_map: &[u8], is_css: bool) {
    if !code.is_empty() && code.last() != Some(&b'\n') {
        code.push(b'\n');
    }
    if is_css {
        code.extend_from_slice(b"/*# sourceMappingURL=data:application/json;base64,");
        code.extend_from_slice(STANDARD.encode(source_map).as_bytes());
        code.extend_from_slice(b" */\n");
    } else {
        code.extend_from_slice(b"//# sourceMappingURL=data:application/json;base64,");
        code.extend_from_slice(STANDARD.encode(source_map).as_bytes());
        code.push(b'\n');
    }
}

fn render_legal_comments(comments: &[String], slash_tag: &str) -> Vec<u8> {
    let mut result = Vec::new();
    for comment in comments {
        result.extend_from_slice(escape_closing_tag(comment, slash_tag).as_bytes());
        if !comment.ends_with('\n') {
            result.push(b'\n');
        }
    }
    result
}

fn default_loader_for_sourcefile(sourcefile: &str) -> Option<Loader> {
    let file_name = FsPath::new(sourcefile)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(sourcefile);
    if file_name.ends_with(".module.css") {
        return Some(Loader::LocalCss);
    }
    match FsPath::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        None | Some("js" | "mjs" | "cjs") => Some(Loader::Js),
        Some("jsx") => Some(Loader::Jsx),
        Some("ts" | "cts" | "mts") => Some(Loader::Ts),
        Some("tsx") => Some(Loader::Tsx),
        Some("css") => Some(Loader::Css),
        Some("json") => Some(Loader::Json),
        Some("txt") => Some(Loader::Text),
        Some(_) => None,
    }
}

fn transform_json(log: &Log, source: Source, options: &TransformOptions) -> Vec<u8> {
    let (expression, ok) =
        js_parser::parse_json(log.clone(), source, js_parser::JsonOptions::default());
    if !ok {
        return Vec::new();
    }
    let renamer = new_no_op_renamer(SymbolMap::new(1));
    let value = js_printer::print_expr(&expression, &renamer, js_printer_options(options));
    export_default(value, options.minify_whitespace)
}

fn transform_text(source: &Source, options: &TransformOptions) -> Vec<u8> {
    let contents = source
        .contents
        .strip_prefix(&[0xef, 0xbb, 0xbf])
        .unwrap_or(&source.contents);
    export_string(contents, options)
}

fn transform_base64(source: &Source, options: &TransformOptions) -> Vec<u8> {
    export_string(STANDARD.encode(&source.contents).as_bytes(), options)
}

fn transform_binary(source: &Source, options: &TransformOptions) -> Vec<u8> {
    let encoded = STANDARD.encode(&source.contents);
    let mut value = b"Uint8Array.fromBase64(".to_vec();
    value.extend(js_printer::quote_utf16(
        &string_to_utf16(encoded.as_bytes()),
        js_printer_options(options),
        true,
    ));
    value.push(b')');
    export_default(value, options.minify_whitespace)
}

fn transform_data_url(source: &Source, options: &TransformOptions) -> Vec<u8> {
    let mime_type = guess_mime_type(&source.pretty_paths.abs, &source.contents);
    let url = encode_string_as_shortest_data_url(&mime_type, &source.contents);
    export_string(url.as_bytes(), options)
}

fn export_string(value: &[u8], options: &TransformOptions) -> Vec<u8> {
    let quoted =
        js_printer::quote_utf16(&string_to_utf16(value), js_printer_options(options), true);
    export_default(quoted, options.minify_whitespace)
}

fn export_default(mut value: Vec<u8>, minify_whitespace: bool) -> Vec<u8> {
    let mut code = if minify_whitespace {
        b"module.exports=".to_vec()
    } else {
        b"module.exports = ".to_vec()
    };
    code.append(&mut value);
    code.extend_from_slice(b";\n");
    code
}

fn js_printer_options(options: &TransformOptions) -> js_printer::Options {
    js_printer::Options {
        line_limit: options.line_limit,
        minify_syntax: options.minify_syntax,
        minify_whitespace: options.minify_whitespace,
        ascii_only: options.ascii_only,
        legal_comments: internal_legal_comments(options.legal_comments, false),
        ..js_printer::Options::default()
    }
}

fn guess_mime_type(sourcefile: &str, contents: &[u8]) -> String {
    let extension = FsPath::new(sourcefile)
        .extension()
        .and_then(|extension| extension.to_str())
        .map_or_else(String::new, |extension| format!(".{extension}"));
    let known = mime_type_by_extension(&extension);
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

fn transform_javascript(log: &Log, source: Source, options: &TransformOptions) -> TransformPrint {
    let line_offset_tables = (options.sourcemap != BuildSourceMap::None)
        .then(|| generate_line_offset_tables(&source.contents, 1));
    let mut parser_options = js_parser::Options::default();
    parser_options.ts.parse = matches!(options.loader, Loader::Ts | Loader::Tsx);
    parser_options.jsx.parse = matches!(options.loader, Loader::Jsx | Loader::Tsx);
    parser_options.jsx.preserve = options.jsx == BuildJsx::Preserve;
    parser_options.jsx.automatic_runtime = options.jsx == BuildJsx::Automatic;
    parser_options.jsx.factory =
        validate_jsx_define(log, &options.jsx_factory, "jsx factory", false);
    parser_options.jsx.fragment =
        validate_jsx_define(log, &options.jsx_fragment, "jsx fragment", true);
    parser_options
        .jsx
        .import_source
        .clone_from(&options.jsx_import_source);
    parser_options.jsx.development = options.jsx_development;
    parser_options.jsx.side_effects = options.jsx_side_effects;
    if !options.tsconfig_raw.is_empty() {
        let file_system = mock_fs(&HashMap::<String, String>::new(), MockKind::Unix, "/");
        if let Some(tsconfig) = parse_tsconfig_raw(log, &file_system, "/", &options.tsconfig_raw) {
            tsconfig.jsx_settings.apply_to(&mut parser_options.jsx);
            parser_options.ts.config = tsconfig.settings;
            parser_options.ts_always_strict =
                tsconfig.ts_always_strict_or_strict().cloned().map(Arc::new);
        }
    }
    parser_options.defines = Some(validate_defines(
        log,
        &options.define,
        &options.pure,
        options.platform,
        options.minify_whitespace && options.minify_identifiers && options.minify_syntax,
    ));
    parser_options.platform = match options.platform {
        BuildPlatform::Default | BuildPlatform::Browser => config::Platform::Browser,
        BuildPlatform::Node => config::Platform::Node,
        BuildPlatform::Neutral => config::Platform::Neutral,
    };
    parser_options.minify_syntax = options.minify_syntax;
    parser_options.minify_identifiers = options.minify_identifiers;
    parser_options.minify_whitespace = options.minify_whitespace;
    parser_options.ascii_only = options.ascii_only;
    parser_options.drop_console = options.drop_console;
    parser_options.drop_debugger = options.drop_debugger;
    parser_options.drop_labels.clone_from(&options.drop_labels);
    parser_options.ignore_dce_annotations = options.ignore_annotations;
    parser_options.keep_names = options.keep_names;
    parser_options.omit_runtime_for_tests = true;
    let (ast, ok) = js_parser::parse(log.clone(), source, parser_options);
    if !ok {
        return TransformPrint::default();
    }
    let mut symbols = SymbolMap::new(1);
    symbols.symbols_for_source[0].clone_from(&ast.symbols);
    let (renamer, helper) = transform_keep_name_renamer(
        &ast,
        symbols,
        options.keep_names,
        options.minify_identifiers,
    );
    let printed = if let Some(line_offset_tables) = line_offset_tables {
        js_printer::print_with_source_map(
            &ast,
            &renamer,
            js_printer_options(options),
            None,
            line_offset_tables,
        )
    } else {
        js_printer::print(&ast, &renamer, js_printer_options(options))
    };
    let mut code = printed.js;
    let printed_len = code.len();
    prepend_keep_name_helper(&mut code, &helper, options.minify_whitespace);
    let source_map_prefix_len = code.len() - printed_len;
    if options.minify_whitespace && !code.is_empty() && code.last() != Some(&b'\n') {
        code.push(b'\n');
    }
    let mut source_map_prefix_offset = LineColumnOffset::default();
    source_map_prefix_offset.advance_bytes(&code[..source_map_prefix_len]);
    let source_map_prefix_lines = usize::try_from(source_map_prefix_offset.lines)
        .expect("source-map prefix line count is non-negative");
    TransformPrint {
        code,
        extracted_legal_comments: printed.extracted_legal_comments,
        source_map_chunk: printed.source_map_chunk,
        source_map_prefix_lines,
    }
}

fn transform_css(log: &Log, source: Source, options: &TransformOptions) -> TransformPrint {
    let identifier_name = source.identifier_name.clone();
    let line_offset_tables = if options.sourcemap == BuildSourceMap::None {
        Vec::new()
    } else {
        generate_line_offset_tables(&source.contents, 1)
    };
    let tree = css_parser::parse(
        log.clone(),
        source,
        css_parser::Options {
            minify_syntax: options.minify_syntax,
            minify_whitespace: options.minify_whitespace,
            minify_identifiers: options.minify_identifiers,
            symbol_mode: match options.loader {
                Loader::LocalCss => css_parser::SymbolMode::Local,
                Loader::GlobalCss => css_parser::SymbolMode::Global,
                _ => css_parser::SymbolMode::Disabled,
            },
        },
    );
    let mut symbols = SymbolMap::new(1);
    symbols.symbols_for_source[0].clone_from(&tree.symbols);
    let local_names = if options.loader == Loader::LocalCss {
        local_css_names(
            &tree,
            &symbols,
            &identifier_name,
            options.minify_identifiers,
        )
    } else {
        HashMap::new()
    };
    let printed = css_printer::print(
        &tree,
        &symbols,
        css_printer::Options {
            local_names,
            line_limit: options.line_limit,
            minify_whitespace: options.minify_whitespace,
            ascii_only: options.ascii_only,
            legal_comments: internal_legal_comments(options.legal_comments, false),
            line_offset_tables,
            source_map: if options.sourcemap == BuildSourceMap::None {
                config::SourceMap::None
            } else {
                config::SourceMap::ExternalWithoutComment
            },
            ..css_printer::Options::default()
        },
    );
    let mut css = printed.css;
    if !css.is_empty() && css.last() != Some(&b'\n') {
        css.push(b'\n');
    }
    TransformPrint {
        code: css,
        extracted_legal_comments: printed.extracted_legal_comments,
        source_map_chunk: printed.source_map_chunk,
        ..TransformPrint::default()
    }
}

fn local_css_names(
    tree: &crate::internal::css_ast::Ast,
    symbols: &SymbolMap,
    identifier_name: &str,
    minify_identifiers: bool,
) -> HashMap<Ref, String> {
    let global_names = tree
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::GlobalCss)
        .map(|symbol| symbol.original_name.clone())
        .collect::<HashSet<_>>();
    let mut references = tree
        .local_symbols
        .iter()
        .map(|loc_ref| loc_ref.reference)
        .collect::<Vec<_>>();
    references.sort_by(|left, right| {
        symbols
            .get(*right)
            .use_count_estimate
            .cmp(&symbols.get(*left).use_count_estimate)
            .then_with(|| left.source_index.cmp(&right.source_index))
            .then_with(|| left.inner_index.cmp(&right.inner_index))
    });

    let mut names = HashMap::new();
    let mut used_names = HashSet::new();
    if minify_identifiers {
        let minifier =
            DEFAULT_NAME_MINIFIER_CSS.shuffle_by_char_freq(tree.char_freq.unwrap_or_default());
        let mut next_name = 0;
        for reference in references {
            let mut name = minifier.number_to_minified_name(next_name);
            while global_names.contains(&name) || used_names.contains(&name) {
                next_name += 1;
                name = minifier.number_to_minified_name(next_name);
            }
            used_names.insert(name.clone());
            names.insert(reference, name);
        }
    } else {
        let mut name_counts = HashMap::<String, u32>::new();
        for reference in references {
            let symbol = symbols.get(reference);
            let mut name = format!("{identifier_name}_{}", symbol.original_name);
            if global_names.contains(&name) || used_names.contains(&name) {
                let prefix = name;
                let tries = name_counts.entry(prefix.clone()).or_insert(1);
                loop {
                    *tries += 1;
                    name = format!("{prefix}{tries}");
                    if !global_names.contains(&name) && !used_names.contains(&name) {
                        break;
                    }
                }
            }
            used_names.insert(name.clone());
            names.insert(reference, name);
        }
    }
    names
}

fn public_messages(messages: Vec<Msg>) -> (Vec<Message>, Vec<Message>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for message in messages {
        let kind = match message.kind {
            MsgKind::Error => MessageKind::Error,
            MsgKind::Warning => MessageKind::Warning,
            MsgKind::Info | MsgKind::Note | MsgKind::Debug | MsgKind::Verbose => continue,
        };
        let public = Message {
            id: msg_id_to_string(message.id).into(),
            plugin_name: message.plugin_name,
            text: message.data.text,
            location: message.data.location.map(public_location),
            notes: message
                .notes
                .into_iter()
                .map(|note| Note {
                    text: note.text,
                    location: note.location.map(public_location),
                })
                .collect(),
            detail: message.data.user_detail,
            kind,
        };
        match kind {
            MessageKind::Error => errors.push(public),
            MessageKind::Warning => warnings.push(public),
        }
    }
    (errors, warnings)
}

fn public_location(location: MsgLocation) -> Location {
    Location {
        file: location.file.rel,
        namespace: location.namespace,
        line: location.line,
        column: location.column,
        length: location.length,
        line_text: String::from_utf8_lossy(&location.line_text).into_owned(),
        suggestion: location.suggestion,
    }
}

fn add_banner_and_footer(mut code: Vec<u8>, banner: &str, footer: &str) -> Vec<u8> {
    if !banner.is_empty() {
        let mut with_banner = Vec::with_capacity(banner.len() + 1 + code.len());
        with_banner.extend_from_slice(banner.as_bytes());
        with_banner.push(b'\n');
        with_banner.append(&mut code);
        code = with_banner;
    }
    if !footer.is_empty() {
        if !code.is_empty() && code.last() != Some(&b'\n') {
            code.push(b'\n');
        }
        code.extend_from_slice(footer.as_bytes());
        code.push(b'\n');
    }
    code
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        BuildEntryPoint, BuildFormat, BuildJsx, BuildLegalComments, BuildOptions, BuildPlatform,
        BuildSourceMap, BuildSourcesContent, BuildStdin, BuildTreeShaking, Loader, Packages,
        TransformOptions, build as build_api, transform,
    };

    fn build(mut options: BuildOptions) -> super::BuildResult {
        options.bundle = true;
        build_api(options)
    }

    fn code(result: super::TransformResult) -> String {
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        String::from_utf8(result.code).expect("transform output is UTF-8")
    }

    #[test]
    fn analyzes_metafiles() {
        let metafile = r#"{
          "inputs": {
            "entry.js": {"bytes": 50, "imports": [{"path": "lib.js"}]},
            "lib.js": {"bytes": 200, "imports": []}
          },
          "outputs": {
            "out.js": {
              "entryPoint": "entry.js",
              "inputs": {
                "entry.js": {"bytesInOutput": 25},
                "lib.js": {"bytesInOutput": 50}
              },
              "bytes": 100
            }
          }
        }"#;
        assert_eq!(
            super::analyze_metafile(metafile, super::AnalyzeMetafileOptions::default()),
            "\n  out.js       100b   100.0%\n   ├ lib.js     50b    50.0%\n   └ entry.js   25b    25.0%\n"
        );
        assert_eq!(
            super::analyze_metafile(
                metafile,
                super::AnalyzeMetafileOptions {
                    verbose: true,
                    ..super::AnalyzeMetafileOptions::default()
                }
            ),
            "\n  out.js ────── 100b ── 100.0%\n   ├ lib.js ──── 50b ─── 50.0%\n   │  └ entry.js\n   └ entry.js ── 25b ─── 25.0%\n"
        );
        assert!(
            super::analyze_metafile("not json", super::AnalyzeMetafileOptions::default())
                .is_empty()
        );
    }

    #[test]
    fn formats_public_messages() {
        let formatted = super::format_messages(
            vec![
                super::Message {
                    text: "This is an error".into(),
                    ..super::Message::default()
                },
                super::Message {
                    text: "Another error".into(),
                    location: Some(super::Location {
                        file: "file.js".into(),
                        ..super::Location::default()
                    }),
                    ..super::Message::default()
                },
            ],
            super::FormatMessagesOptions::default(),
        );
        assert_eq!(formatted.len(), 2);
        assert_eq!(
            formatted[0],
            format!(
                "{} [ERROR] This is an error\n\n",
                crate::internal::logger::MsgKind::Error.icon()
            )
        );
        assert_eq!(
            formatted[1],
            format!(
                "{} [ERROR] Another error\n\n    file.js:0:0:\n      0 │ \n        ╵ ^\n\n",
                crate::internal::logger::MsgKind::Error.icon()
            )
        );
    }

    #[test]
    fn defaults_build_api_to_pass_through_mode() {
        let result = super::build(BuildOptions {
            stdin: Some(BuildStdin {
                contents: "import {value} from './dependency.js'; const unused = 1; use(value);"
                    .into(),
                sourcefile: "entry.js".into(),
                ..BuildStdin::default()
            }),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(
            output.contains("import { value } from \"./dependency.js\";"),
            "{output}"
        );
        assert!(output.contains("const unused = 1;"), "{output}");
    }

    #[test]
    fn validates_bundle_only_build_options() {
        let external = super::build(BuildOptions {
            external: vec!["pkg".into()],
            ..BuildOptions::default()
        });
        assert_eq!(
            external.errors.first().map(|message| message.text.as_str()),
            Some("Cannot use \"external\" without \"bundle\"")
        );

        let alias = super::build(BuildOptions {
            alias: HashMap::from([("old".into(), "new".into())]),
            ..BuildOptions::default()
        });
        assert_eq!(
            alias.errors.first().map(|message| message.text.as_str()),
            Some("Cannot use \"alias\" without \"bundle\"")
        );
    }

    #[test]
    fn validates_build_output_topology() {
        let multiple = super::build(BuildOptions {
            entry_points: vec!["a.js".into(), "b.js".into()],
            ..BuildOptions::default()
        });
        assert_eq!(
            multiple.errors.first().map(|message| message.text.as_str()),
            Some("Must use \"outdir\" when there are multiple input files")
        );

        let splitting_without_outdir = super::build(BuildOptions {
            splitting: true,
            ..BuildOptions::default()
        });
        assert_eq!(
            splitting_without_outdir
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Must use \"outdir\" when code splitting is enabled")
        );

        let conflicting = super::build(BuildOptions {
            outdir: "out".into(),
            outfile: "out.js".into(),
            ..BuildOptions::default()
        });
        assert_eq!(
            conflicting
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Cannot use both \"outfile\" and \"outdir\"")
        );

        let splitting_iife = super::build(BuildOptions {
            bundle: true,
            splitting: true,
            outdir: "out".into(),
            ..BuildOptions::default()
        });
        assert_eq!(
            splitting_iife
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Splitting currently only works with the \"esm\" format")
        );
    }

    #[test]
    fn validates_build_options_that_need_output_paths() {
        let source_map = super::build(BuildOptions {
            sourcemap: BuildSourceMap::External,
            ..BuildOptions::default()
        });
        assert_eq!(
            source_map
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Cannot use an external source map without an output path")
        );

        let legal_comments = super::build(BuildOptions {
            legal_comments: BuildLegalComments::Linked,
            ..BuildOptions::default()
        });
        assert_eq!(
            legal_comments
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Cannot use linked or external legal comments without an output path")
        );

        for (loader, expected) in [
            (
                Loader::File,
                "Cannot use the \"file\" loader without an output path",
            ),
            (
                Loader::Copy,
                "Cannot use the \"copy\" loader without an output path",
            ),
        ] {
            let result = super::build(BuildOptions {
                loader: HashMap::from([(".asset".into(), loader)]),
                ..BuildOptions::default()
            });
            assert_eq!(
                result.errors.first().map(|message| message.text.as_str()),
                Some(expected)
            );
        }
    }

    #[test]
    fn hashes_in_memory_output_files() {
        assert_eq!(super::output_file_hash(b"hello"), "o22fiH2CxyY");
        let result = build(BuildOptions {
            stdin: Some(BuildStdin {
                contents: "console.log('hash')".into(),
                sourcefile: "input.js".into(),
                ..BuildStdin::default()
            }),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        let output = &result.output_files[0];
        assert_eq!(output.hash, super::output_file_hash(&output.contents));
        assert_eq!(output.hash.len(), 11);
    }

    #[test]
    fn optionally_writes_build_outputs_to_disk() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-api-write-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        let output = directory.join("nested/out.js");
        std::fs::write(&entry, "#!/usr/bin/env node\nconsole.log('written')")
            .expect("write entry file");

        let in_memory = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outfile: "nested/out.js".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            ..BuildOptions::default()
        });
        assert!(in_memory.errors.is_empty(), "{:?}", in_memory.errors);
        assert_eq!(in_memory.output_files.len(), 1);
        assert!(!output.exists());

        let written = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outfile: "nested/out.js".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            write: true,
            ..BuildOptions::default()
        });
        assert!(written.errors.is_empty(), "{:?}", written.errors);
        assert_eq!(written.output_files.len(), 1);
        assert_eq!(
            std::fs::read(&output).expect("read written output"),
            written.output_files[0].contents
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&output)
                .expect("read output metadata")
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
        }

        let protected = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outfile: "entry.js".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            write: true,
            ..BuildOptions::default()
        });
        assert!(
            protected
                .errors
                .iter()
                .any(|error| error.text.contains("Refusing to overwrite input file")),
            "{:?}",
            protected.errors
        );
        assert_eq!(
            std::fs::read_to_string(&entry).expect("read protected entry"),
            "#!/usr/bin/env node\nconsole.log('written')"
        );

        let blocked = directory.join("blocked");
        std::fs::write(&blocked, "not a directory").expect("write blocking file");
        let failed = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outfile: "blocked/out.js".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            write: true,
            ..BuildOptions::default()
        });
        assert!(
            failed
                .errors
                .iter()
                .any(|error| error.text.contains("Could not create output directory")),
            "{:?}",
            failed.errors
        );

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn builds_filesystem_entry_points_to_memory() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-api-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(directory.join("entry.js"), "console.log('api build')")
            .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        assert!(result.output_files[0].path.ends_with("/out/entry.js"));
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("console.log(\"api build\");"));
        assert!(output.starts_with("(() => {\n"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn builds_stdin_as_a_synthetic_entry_point() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-build-stdin-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("dependency.ts"),
            "export const value: number = 42",
        )
        .expect("write dependency");

        let result = build(BuildOptions {
            stdin: Some(BuildStdin {
                contents:
                    "import {value} from './dependency.ts'; const result: number = value; console.log(result)"
                        .into(),
                resolve_dir: ".".into(),
                sourcefile: "virtual-entry.ts".into(),
                loader: Loader::Ts,
            }),
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        assert!(result.output_files[0].path.ends_with("/out/stdin.js"));
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("const value = 42;"));
        assert!(output.contains("const result = value;"));
        assert!(output.contains("console.log(result);"));
        assert!(!output.contains("import "));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn controls_symlink_identity_during_resolution() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-symlinks-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import './real.js'; import './alias.js'",
        )
        .expect("write entry file");
        std::fs::write(directory.join("real.js"), "console.log('dependency')")
            .expect("write dependency file");
        std::os::unix::fs::symlink(directory.join("real.js"), directory.join("alias.js"))
            .expect("create symlink");

        for (preserve_symlinks, expected_count) in [(false, 1), (true, 2)] {
            let result = build(BuildOptions {
                entry_points: vec!["entry.js".into()],
                outdir: "out".into(),
                abs_working_dir: directory.to_string_lossy().into_owned(),
                preserve_symlinks,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            let output = String::from_utf8_lossy(&result.output_files[0].contents);
            assert_eq!(
                output.matches("console.log(\"dependency\")").count(),
                expected_count
            );
        }
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn returns_bundle_metafile_with_input_and_output_details() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-metafile-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './dep.js'; import external from 'pkg'; console.log(value, external)",
        )
        .expect("write entry file");
        std::fs::write(directory.join("dep.js"), "export const value = 42")
            .expect("write dependency file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            external: vec!["pkg".into()],
            metafile: true,
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        let log = crate::internal::logger::Log::new_defer(
            crate::internal::logger::DeferLogKind::All,
            HashMap::new(),
        );
        let (_, valid) = crate::internal::js_parser::parse_json(
            log.clone(),
            crate::internal::logger::Source {
                contents: std::sync::Arc::from(result.metafile.as_bytes()),
                ..crate::internal::logger::Source::default()
            },
            crate::internal::js_parser::JsonOptions::default(),
        );
        assert!(valid);
        assert!(log.done().is_empty());
        assert!(result.metafile.contains("\"entry.js\": {"));
        assert!(result.metafile.contains("\"dep.js\": {"));
        assert!(result.metafile.contains("\"original\": \"./dep.js\""));
        assert!(result.metafile.contains("\"path\": \"pkg\""));
        assert!(result.metafile.contains("\"external\": true"));
        assert!(result.metafile.contains("\"out/entry.js\": {"));
        assert!(result.metafile.contains("\"bytesInOutput\":"));
        assert!(result.metafile.ends_with("}\n"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn preserves_entry_directories_relative_to_outbase() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-outbase-{unique}"));
        std::fs::create_dir_all(directory.join("src/pages")).expect("create pages directory");
        std::fs::create_dir_all(directory.join("src/admin")).expect("create admin directory");
        std::fs::write(directory.join("src/pages/home.js"), "console.log('home')")
            .expect("write home entry");
        std::fs::write(directory.join("src/admin/panel.js"), "console.log('panel')")
            .expect("write panel entry");

        let result = build(BuildOptions {
            entry_points: vec!["src/pages/home.js".into(), "src/admin/panel.js".into()],
            outdir: "out".into(),
            outbase: "src".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let paths = result
            .output_files
            .iter()
            .map(|output| output.path.as_str())
            .collect::<Vec<_>>();
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("/out/pages/home.js"))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("/out/admin/panel.js"))
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn supports_advanced_entry_point_output_paths() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-advanced-entry-{unique}"));
        std::fs::create_dir_all(directory.join("src")).expect("create source directory");
        std::fs::write(
            directory.join("src/app.js"),
            "console.log('advanced entry')",
        )
        .expect("write entry");

        let result = build(BuildOptions {
            entry_points_advanced: vec![BuildEntryPoint {
                input_path: "src/app.js".into(),
                output_path: "custom/nested/application".into(),
            }],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        assert!(
            result.output_files[0]
                .path
                .ends_with("/out/custom/nested/application.js")
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn resolves_packages_from_configured_node_paths() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-node-path-{unique}"));
        let global_modules = directory.join("global_modules");
        std::fs::create_dir_all(global_modules.join("pkg")).expect("create global package");
        std::fs::write(
            directory.join("entry.js"),
            "import {value} from 'pkg'; console.log(value)",
        )
        .expect("write entry");
        std::fs::write(
            global_modules.join("pkg/package.json"),
            r#"{"exports":"./main.js"}"#,
        )
        .expect("write package manifest");
        std::fs::write(
            global_modules.join("pkg/main.js"),
            "export const value = 'global node path'",
        )
        .expect("write package module");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            node_paths: vec!["global_modules".into()],
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("\"global node path\""));
        assert!(!output.contains("from \"pkg\""));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn applies_entry_chunk_and_asset_name_templates() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-name-templates-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import('./dep.js').then(ns => console.log(ns.image))",
        )
        .expect("write entry file");
        std::fs::write(
            directory.join("dep.js"),
            "import image from './image.asset'; export {image}",
        )
        .expect("write dependency");
        std::fs::write(directory.join("image.asset"), b"named asset").expect("write asset");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            splitting: true,
            loader: HashMap::from([(".asset".into(), Loader::File)]),
            entry_names: "entries/[name]-[hash]".into(),
            chunk_names: "chunks/[name]-[hash]".into(),
            asset_names: "assets/[name]-[hash]".into(),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 3);
        assert!(result.output_files.iter().any(|output| {
            output.path.contains("/out/entries/entry-")
                && std::path::Path::new(&output.path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("js"))
        }));
        assert!(result.output_files.iter().any(|output| {
            output.path.contains("/out/chunks/dep-")
                && std::path::Path::new(&output.path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("js"))
        }));
        assert!(result.output_files.iter().any(|output| {
            output.path.contains("/out/assets/image-")
                && std::path::Path::new(&output.path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("asset"))
        }));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn emits_linked_build_source_maps() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-sourcemap-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "const sourceMapValue = 1; console.log(sourceMapValue)",
        )
        .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            sourcemap: BuildSourceMap::Linked,
            source_root: "https://cdn.example/source/".into(),
            sources_content: BuildSourcesContent::Exclude,
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 2);
        let javascript = result
            .output_files
            .iter()
            .find(|output| output.path.ends_with("/entry.js"))
            .expect("JavaScript output");
        let source_map = result
            .output_files
            .iter()
            .find(|output| output.path.ends_with("/entry.js.map"))
            .expect("source map output");
        assert!(
            String::from_utf8_lossy(&javascript.contents)
                .contains("//# sourceMappingURL=entry.js.map")
        );
        let source_map = String::from_utf8_lossy(&source_map.contents);
        assert!(source_map.contains("\"version\": 3"));
        assert!(source_map.contains("entry.js"));
        assert!(source_map.contains("\"sourceRoot\": \"https://cdn.example/source/\""));
        assert!(!source_map.contains("\"sourcesContent\""));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn emits_all_build_source_map_modes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-sourcemap-modes-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "console.log('source map modes')",
        )
        .expect("write entry file");

        for (mode, name, output_count, has_inline_map) in [
            (BuildSourceMap::External, "external", 2, false),
            (BuildSourceMap::Inline, "inline", 1, true),
            (BuildSourceMap::InlineAndExternal, "both", 2, true),
        ] {
            let result = build(BuildOptions {
                entry_points: vec!["entry.js".into()],
                outdir: format!("out-{name}"),
                abs_working_dir: directory.to_string_lossy().into_owned(),
                format: BuildFormat::EsModule,
                sourcemap: mode,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            assert_eq!(result.output_files.len(), output_count);
            let javascript = result
                .output_files
                .iter()
                .find(|output| output.path.ends_with("/entry.js"))
                .expect("JavaScript output");
            let javascript = String::from_utf8_lossy(&javascript.contents);
            assert_eq!(
                javascript.contains("//# sourceMappingURL=data:application/json;base64,"),
                has_inline_map
            );
            assert!(!javascript.contains("//# sourceMappingURL=entry.js.map"));
        }
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn emits_linked_legal_comment_files() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-legal-comments-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "/*! important license */ const value = 1; console.log(value)",
        )
        .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            legal_comments: BuildLegalComments::Linked,
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 2);
        let javascript = result
            .output_files
            .iter()
            .find(|output| output.path.ends_with("/entry.js"))
            .expect("JavaScript output");
        let legal = result
            .output_files
            .iter()
            .find(|output| output.path.ends_with("/entry.js.LEGAL.txt"))
            .expect("legal comments output");
        assert!(
            String::from_utf8_lossy(&javascript.contents)
                .contains("For license information please see entry.js.LEGAL.txt")
        );
        assert_eq!(
            String::from_utf8_lossy(&legal.contents),
            "/*! important license */\n"
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn preserves_configured_external_imports() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-external-{unique}"));
        std::fs::create_dir_all(directory.join("src")).expect("create source directory");
        std::fs::create_dir_all(directory.join("vendor")).expect("create vendor directory");
        std::fs::write(
            directory.join("src/entry.js"),
            "import one from 'pkg/subpath'; import two from '../vendor/tool.js'; console.log(one, two)",
        )
        .expect("write entry file");
        std::fs::write(
            directory.join("vendor/tool.js"),
            "export default 'must stay external'",
        )
        .expect("write vendor file");

        let result = build(BuildOptions {
            entry_points: vec!["src/entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            external: vec!["pkg".into(), "./vendor/*".into()],
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("from \"pkg/subpath\""));
        assert!(output.contains("from \"../vendor/tool.js\""));
        assert!(!output.contains("must stay external"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn can_externalize_all_package_imports() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-packages-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import value from 'missing-package'; console.log(value)",
        )
        .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            packages: Packages::External,
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("from \"missing-package\""));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn externalizes_node_builtins_for_node_platform_builds() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-node-platform-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import fs from 'node:fs'; console.log(fs.readFileSync)",
        )
        .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::CommonJs,
            platform: BuildPlatform::Node,
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("require(\"node:fs\")"));
        assert!(output.contains("readFileSync"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn configures_package_fields_and_resolve_extensions() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-resolve-options-{unique}"));
        std::fs::create_dir_all(directory.join("node_modules/pkg"))
            .expect("create package directory");
        std::fs::create_dir_all(directory.join("node_modules/conditional"))
            .expect("create conditional package directory");
        std::fs::write(
            directory.join("entry.js"),
            "import {choice} from 'pkg'; import condition from 'conditional'; import {custom} from './local'; console.log(choice, condition, custom)",
        )
        .expect("write entry file");
        std::fs::write(
            directory.join("node_modules/pkg/package.json"),
            r#"{"main":"main.js","module":"module.js"}"#,
        )
        .expect("write package metadata");
        std::fs::write(
            directory.join("node_modules/pkg/main.js"),
            "export const choice = 'main-field-choice'",
        )
        .expect("write main module");
        std::fs::write(
            directory.join("node_modules/pkg/module.js"),
            "export const choice = 'module-field-choice'",
        )
        .expect("write module module");
        std::fs::write(
            directory.join("local.custom"),
            "export const custom = 'custom-extension-choice'",
        )
        .expect("write custom module");
        std::fs::write(
            directory.join("node_modules/conditional/package.json"),
            r#"{"exports":{".":{"custom":"./custom.js","default":"./default.js"}}}"#,
        )
        .expect("write conditional package metadata");
        std::fs::write(
            directory.join("node_modules/conditional/custom.js"),
            "export default 'custom-condition-choice'",
        )
        .expect("write custom condition module");
        std::fs::write(
            directory.join("node_modules/conditional/default.js"),
            "export default 'default-condition-choice'",
        )
        .expect("write default condition module");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            loader: HashMap::from([(".custom".into(), Loader::Js)]),
            main_fields: vec!["main".into()],
            resolve_extensions: vec![".custom".into(), ".js".into()],
            conditions: vec!["custom".into()],
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("main-field-choice"));
        assert!(!output.contains("module-field-choice"));
        assert!(output.contains("custom-extension-choice"));
        assert!(output.contains("custom-condition-choice"));
        assert!(!output.contains("default-condition-choice"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn assigns_iife_exports_to_a_global_name() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-global-name-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(directory.join("entry.js"), "export const value = 123")
            .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            global_name: "My.Library".into(),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("var My"));
        assert!(output.contains("(My ||= {}).Library ="), "{output}");
        assert!(output.contains("value: () => value"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn rejects_invalid_global_names() {
        let result = build(BuildOptions {
            global_name: "not/a/global".into(),
            ..BuildOptions::default()
        });
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn applies_build_loader_overrides() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-loader-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import message from './message.custom'; console.log(message)",
        )
        .expect("write entry file");
        std::fs::write(directory.join("message.custom"), "hello loader")
            .expect("write custom input");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            loader: HashMap::from([(".custom".into(), Loader::Text)]),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("\"hello loader\""));
        assert!(output.contains("console.log("));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn exposes_build_jsx_modes_and_configuration() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-jsx-options-{unique}"));
        std::fs::create_dir_all(directory.join("node_modules/preact"))
            .expect("create package directory");
        let entry = directory.join("entry.jsx");
        std::fs::write(&entry, "console.log(<><div /></>)").expect("write entry file");

        let common = || BuildOptions {
            entry_points: vec!["entry.jsx".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            ..BuildOptions::default()
        };

        let preserved = build(BuildOptions {
            jsx: BuildJsx::Preserve,
            ..common()
        });
        assert!(preserved.errors.is_empty(), "{:?}", preserved.errors);
        let output = String::from_utf8_lossy(&preserved.output_files[0].contents);
        assert!(output.contains("<>"));
        assert!(output.contains("<div />"));

        let transformed = build(BuildOptions {
            jsx_factory: "h".into(),
            jsx_fragment: "Frag".into(),
            ..common()
        });
        assert!(transformed.errors.is_empty(), "{:?}", transformed.errors);
        let output = String::from_utf8_lossy(&transformed.output_files[0].contents);
        assert!(output.contains("h(Frag, null"), "{output}");
        assert!(output.contains("h(\"div\", null)"), "{output}");

        std::fs::write(
            directory.join("node_modules/preact/package.json"),
            r#"{"sideEffects":false}"#,
        )
        .expect("write package metadata");
        std::fs::write(
            directory.join("node_modules/preact/jsx-runtime.js"),
            "export function jsx(type, props) { return {type, props} }",
        )
        .expect("write automatic runtime");
        std::fs::write(&entry, "console.log(<div />)").expect("write automatic entry");
        let automatic = build(BuildOptions {
            jsx: BuildJsx::Automatic,
            jsx_import_source: "preact".into(),
            ..common()
        });
        assert!(automatic.errors.is_empty(), "{:?}", automatic.errors);
        let output = String::from_utf8_lossy(&automatic.output_files[0].contents);
        assert!(output.contains("function jsx(type, props)"), "{output}");
        assert!(output.contains("jsx(\"div\", {})"), "{output}");

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn substitutes_build_defines() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-defines-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "console.log(process.env.NODE_ENV, process[\"env\"][\"NODE_ENV\"], DEBUG)",
        )
        .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            define: HashMap::from([
                ("process.env.NODE_ENV".into(), r#""production""#.into()),
                ("DEBUG".into(), "false".into()),
            ]),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(
            output.contains("console.log(\"production\", \"production\", false)"),
            "{output}"
        );
        assert!(!output.contains("process.env.NODE_ENV"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn supports_compound_build_defines() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-compound-defines-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "console.log(CONFIG === CONFIG, CONFIG.nested[1])",
        )
        .expect("write entry file");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            define: HashMap::from([("CONFIG".into(), r#"{"nested":[1,2]}"#.into())]),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("var define_CONFIG_default = {"));
        assert!(output.contains("nested: [1, 2]"));
        assert!(
            output.contains(
                "console.log(define_CONFIG_default === define_CONFIG_default, define_CONFIG_default.nested[1])"
            ),
            "{output}"
        );
        assert_eq!(output.matches("nested: [1, 2]").count(), 1);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn substitutes_transform_defines() {
        let scalar = transform(
            "console.log(process.env.NODE_ENV, process['env']['NODE_ENV'], DEBUG)",
            TransformOptions {
                define: HashMap::from([
                    ("process.env.NODE_ENV".into(), r#""production""#.into()),
                    ("DEBUG".into(), "false".into()),
                ]),
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            code(scalar),
            "console.log(\"production\", \"production\", false);\n"
        );

        let compound = transform(
            "console.log(CONFIG === CONFIG, CONFIG.nested[1])",
            TransformOptions {
                define: HashMap::from([("CONFIG".into(), r#"{"nested":[1,2]}"#.into())]),
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            code(compound),
            "var define_CONFIG_default = { nested: [1, 2] };\nconsole.log(define_CONFIG_default === define_CONFIG_default, define_CONFIG_default.nested[1]);\n"
        );

        let invalid = transform(
            "console.log(DEBUG)",
            TransformOptions {
                define: HashMap::from([("DEBUG".into(), "1 + 2".into())]),
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            invalid.errors.first().map(|message| message.text.as_str()),
            Some("Invalid define value (must be an entity name or JS literal): 1 + 2")
        );
        assert!(invalid.code.is_empty());
    }

    #[test]
    fn preserves_single_line_if_body_formatting() {
        assert_eq!(
            code(transform("if (a) b()", TransformOptions::default())),
            "if (a) b();\n"
        );
        assert_eq!(
            code(transform(
                "if (a) b(); else c()",
                TransformOptions::default()
            )),
            "if (a) b();\nelse c();\n"
        );
        assert_eq!(
            code(transform(
                "if (a) { b() } else c()",
                TransformOptions::default()
            )),
            "if (a) {\n  b();\n} else c();\n"
        );
        assert_eq!(
            code(transform(
                "if (DEBUG) console.log('x')",
                TransformOptions {
                    define: HashMap::from([("DEBUG".into(), "false".into())]),
                    ..TransformOptions::default()
                }
            )),
            "if (false) console.log(\"x\");\n"
        );
        assert_eq!(
            code(transform(
                "if(a){b()}else if(c)d();while(y)z();do q();while(r)",
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "if(a){b()}else if(c)d();while(y)z();do q();while(r);\n"
        );
    }

    #[test]
    fn transforms_optional_catch_bindings_and_identifier_arrow_statements() {
        let source = "try{x()}catch{y()}\nx=>({a:x})";
        assert_eq!(
            code(transform(source, TransformOptions::default())),
            "try {\n  x();\n} catch {\n  y();\n}\n(x2) => ({ a: x2 });\n"
        );
        assert_eq!(
            code(transform(
                source,
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "try{x()}catch{y()}x2=>({a:x2});\n"
        );
        assert_eq!(
            code(transform(
                source,
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "try {\n  x();\n} catch {\n  y();\n}\n"
        );
        assert_eq!(
            code(transform(
                "const f=x=>({a:x})",
                TransformOptions {
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "const f = (a) => ({ a });\n"
        );
    }

    #[test]
    fn minifies_computed_primitive_object_keys() {
        assert_eq!(
            code(transform(
                r#"const f=(a,b,c)=>({["a"]:a,[1]:b,["__proto__"]:c})"#,
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "const f = (a, b, c) => ({ a, 1: b, [\"__proto__\"]: c });\n"
        );
        let enum_code = code(transform(
            "enum E { X = \"x\", N = 1 }\nconst f=(a,b)=>({[E.X]:a,[E.N]:b})",
            TransformOptions {
                loader: Loader::Ts,
                minify_syntax: true,
                ..TransformOptions::default()
            },
        ));
        assert!(
            enum_code.ends_with("const f = (a, b) => ({ x: a, 1: b });\n"),
            "{enum_code}"
        );
        assert_eq!(
            code(transform(
                r#"x["foo"];x["x-y"];x["default"]"#,
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "x.foo, x[\"x-y\"], x.default;\n"
        );
        assert_eq!(
            code(transform(
                r#"x?.["foo"];const g=()=>{x();return y}"#,
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "x?.foo;\nconst g = () => (x(), y);\n"
        );
        assert_eq!(
            enum_code,
            "var E = /* @__PURE__ */ ((E2) => (E2.X = \"x\", E2[E2.N = 1] = \"N\", E2))(E || {});\nconst f = (a, b) => ({ x: a, 1: b });\n"
        );
        assert_eq!(
            code(transform(
                "enum E { X = \"x\", N = 1 }\nconst f=(a,b)=>({[E.X]:a,[E.N]:b})",
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "var E=(n=>(n.X=\"x\",n[n.N=1]=\"N\",n))(E||{});const f=(N,X)=>({x:N,1:X});\n"
        );
        assert_eq!(
            code(transform(
                "enum E { X = \"x\" };const value=E.X",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "var E = /* @__PURE__ */ ((E2) => {\n  E2[\"X\"] = \"x\";\n  return E2;\n})(E || {});\n;\nconst value = \"x\" /* X */;\n"
        );
    }

    #[test]
    fn renames_and_merges_typescript_enums_like_esbuild() {
        let collision = "enum foo{foo=123,bar=foo};console.log(foo)";
        assert_eq!(
            code(transform(
                collision,
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "var foo = /* @__PURE__ */ ((_foo) => {\n  _foo[_foo[\"foo\"] = 123] = \"foo\";\n  _foo[_foo[\"bar\"] = 123 /* foo */] = \"bar\";\n  return _foo;\n})(foo || {});\n;\nconsole.log(foo);\n"
        );
        assert_eq!(
            code(transform(
                collision,
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "var foo = /* @__PURE__ */ ((_foo) => (_foo[_foo.foo = 123] = \"foo\", _foo[_foo.bar = 123 /* foo */] = \"bar\", _foo))(foo || {});\nconsole.log(foo);\n"
        );
        assert_eq!(
            code(transform(
                collision,
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "var foo=(o=>(o[o.foo=123]=\"foo\",o[o.bar=123]=\"bar\",o))(foo||{});console.log(foo);\n"
        );

        let adjacent = "enum E{A};enum F{B};console.log(E,F)";
        assert_eq!(
            code(transform(
                adjacent,
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "var E = /* @__PURE__ */ ((E2) => (E2[E2.A = 0] = \"A\", E2))(E || {}), F = /* @__PURE__ */ ((F2) => (F2[F2.B = 0] = \"B\", F2))(F || {});\nconsole.log(E, F);\n"
        );
        assert_eq!(
            code(transform(
                adjacent,
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "var E=(e=>(e[e.A=0]=\"A\",e))(E||{}),F=(e=>(e[e.B=0]=\"B\",e))(F||{});console.log(E,F);\n"
        );
    }

    #[test]
    fn defaults_node_env_for_browser_transforms() {
        let development = code(transform(
            "console.log(process.env.NODE_ENV)",
            TransformOptions::default(),
        ));
        assert!(development.contains("\"development\""), "{development}");

        let production = code(transform(
            "console.log(process.env.NODE_ENV)",
            TransformOptions {
                minify_whitespace: true,
                minify_identifiers: true,
                minify_syntax: true,
                ..TransformOptions::default()
            },
        ));
        assert!(production.contains("\"production\""), "{production}");

        let node = code(transform(
            "console.log(process.env.NODE_ENV)",
            TransformOptions {
                platform: BuildPlatform::Node,
                ..TransformOptions::default()
            },
        ));
        assert!(node.contains("process.env.NODE_ENV"), "{node}");
    }

    #[test]
    fn defaults_public_options_to_ascii_charset() {
        assert!(TransformOptions::default().ascii_only);
        assert!(BuildOptions::default().ascii_only);
        assert_eq!(
            code(transform("\"π😀\"", TransformOptions::default())),
            "\"\\u03C0\\u{1F600}\";\n"
        );
        assert_eq!(
            code(transform(
                "\"π😀\"",
                TransformOptions {
                    ascii_only: false,
                    ..TransformOptions::default()
                }
            )),
            "\"π😀\";\n"
        );
    }

    #[test]
    fn minifies_constant_if_statements() {
        let options = TransformOptions {
            minify_syntax: true,
            ..TransformOptions::default()
        };
        assert_eq!(
            code(transform(
                "if (true) { console.log('yes') }",
                options.clone()
            )),
            "console.log(\"yes\");\n"
        );
        assert_eq!(
            code(transform(
                "if (false) console.log('yes'); else console.log('no')",
                options.clone()
            )),
            "console.log(\"no\");\n"
        );
        assert_eq!(
            code(transform("if (false) console.log('never')", options)),
            ""
        );
        assert_eq!(
            code(transform(
                "if (true) { console.log('yes') }",
                TransformOptions::default()
            )),
            "if (true) {\n  console.log(\"yes\");\n}\n"
        );
    }

    #[test]
    fn coalesces_adjacent_local_declarations_when_minifying_syntax() {
        assert_eq!(
            code(transform(
                "const first = `hello ${name}!`; const second = tag`a${value}c`;",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "const first = `hello ${name}!`, second = tag`a${value}c`;\n"
        );
        assert_eq!(
            code(transform(
                "let first = 1; let second = 2; const third = 3;",
                TransformOptions {
                    minify_whitespace: true,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "let first=1,second=2;const third=3;\n"
        );
    }

    #[test]
    fn folds_binary_constants_when_minifying_syntax() {
        assert_eq!(
            code(transform(
                "const sum = 1 + 2 * 3; const shifted = 8 << 2; const text = 'a' + 'b';",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "const sum = 7, shifted = 32, text = \"ab\";\n"
        );
    }

    #[test]
    fn minifies_transform_identifiers_by_frequency() {
        assert_eq!(
            code(transform(
                "function longName(longArgument) { let localValue = longArgument; return localValue }",
                TransformOptions {
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "function longName(l) {\n  let e = l;\n  return e;\n}\n"
        );
        assert_eq!(
            code(transform(
                "function longName(longArgument) { return externalValue + longArgument }",
                TransformOptions {
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "function longName(e) {\n  return externalValue + e;\n}\n"
        );
        assert_eq!(
            code(transform(
                "const fn = ({x}) => ({x})",
                TransformOptions {
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "const fn = ({ x: n }) => ({ x: n });\n"
        );
        assert_eq!(
            code(transform(
                "import def, {x as y, z} from 'pkg'; console.log(def, y, z)",
                TransformOptions {
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "import o, { x as e, z as f } from \"pkg\";\nconsole.log(o, e, f);\n"
        );
    }

    #[test]
    fn minifies_transform_whitespace_like_upstream() {
        assert_eq!(
            code(transform(
                "function f(a) { return a + 1 }",
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "function f(a){return a+1}\n"
        );
        assert_eq!(
            code(transform(
                "function f() { foo(); let x = 1; bar(x) }",
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "function f(){foo();let x=1;bar(x)}\n"
        );
        assert_eq!(
            code(transform(
                "import data from './x.json' with {type: 'json'}; console.log(data)",
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "import data from\"./x.json\"with{type:\"json\"};console.log(data);\n"
        );
    }

    #[test]
    fn defaults_node_env_for_browser_builds() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-node-env-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "console.log(process.env.NODE_ENV)",
        )
        .expect("write entry file");

        let build_with = |minify: bool| {
            build(BuildOptions {
                entry_points: vec!["entry.js".into()],
                outdir: "out".into(),
                abs_working_dir: directory.to_string_lossy().into_owned(),
                format: BuildFormat::Iife,
                minify_whitespace: minify,
                minify_identifiers: minify,
                minify_syntax: minify,
                ..BuildOptions::default()
            })
        };
        let development = build_with(false);
        let production = build_with(true);
        assert!(development.errors.is_empty(), "{:?}", development.errors);
        assert!(production.errors.is_empty(), "{:?}", production.errors);
        assert!(
            String::from_utf8_lossy(&development.output_files[0].contents)
                .contains("\"development\"")
        );
        assert!(
            String::from_utf8_lossy(&production.output_files[0].contents)
                .contains("\"production\"")
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn emits_files_for_the_file_loader() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-file-loader-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import asset from './image.asset'; console.log(asset)",
        )
        .expect("write entry file");
        std::fs::write(directory.join("image.asset"), b"binary asset contents")
            .expect("write asset");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::Iife,
            loader: HashMap::from([(".asset".into(), Loader::File)]),
            public_path: "https://cdn.example/assets".into(),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 2);
        let javascript = result
            .output_files
            .iter()
            .find(|output| output.path.ends_with("/entry.js"))
            .expect("JavaScript output");
        let asset = result
            .output_files
            .iter()
            .find(|output| !output.path.ends_with("/entry.js"))
            .expect("asset output");
        assert_eq!(asset.contents, b"binary asset contents");
        let asset_name = std::path::Path::new(&asset.path)
            .file_name()
            .expect("asset file name")
            .to_string_lossy();
        assert!(asset_name.starts_with("image-"));
        assert!(asset_name.ends_with(".asset"));
        let javascript = String::from_utf8_lossy(&javascript.contents);
        assert!(javascript.contains(asset_name.as_ref()));
        assert!(javascript.contains("https://cdn.example/assets/"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn rewrites_imports_for_the_copy_loader() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-copy-loader-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "import './worker.copy'; console.log('copy loader')",
        )
        .expect("write entry file");
        std::fs::write(directory.join("worker.copy"), b"copied worker contents")
            .expect("write copied input");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            loader: HashMap::from([(".copy".into(), Loader::Copy)]),
            ..BuildOptions::default()
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 2);
        let javascript = result
            .output_files
            .iter()
            .find(|output| output.path.ends_with("/entry.js"))
            .expect("JavaScript output");
        let copied = result
            .output_files
            .iter()
            .find(|output| !output.path.ends_with("/entry.js"))
            .expect("copied output");
        assert_eq!(copied.contents, b"copied worker contents");
        let copied_name = std::path::Path::new(&copied.path)
            .file_name()
            .expect("copied file name")
            .to_string_lossy();
        assert!(copied_name.starts_with("worker-"));
        assert!(String::from_utf8_lossy(&javascript.contents).contains(copied_name.as_ref()));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn rejects_external_paths_with_multiple_wildcards() {
        let result = build(BuildOptions {
            external: vec!["pkg/*/bad/*".into()],
            ..BuildOptions::default()
        });
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].text.contains("more than one"));
    }

    #[test]
    fn exposes_build_tree_shaking_control() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-api-tree-shaking-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "const dead = 1; console.log('live')").expect("write entry file");

        for (tree_shaking, should_keep_dead) in [
            (BuildTreeShaking::Enabled, false),
            (BuildTreeShaking::Disabled, true),
        ] {
            let result = build(BuildOptions {
                entry_points: vec![entry.to_string_lossy().into_owned()],
                outdir: directory.join("out").to_string_lossy().into_owned(),
                tree_shaking,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            let output = String::from_utf8_lossy(&result.output_files[0].contents);
            assert_eq!(
                output.contains("dead"),
                should_keep_dead,
                "{tree_shaking:?}"
            );
            assert!(output.contains("console.log(\"live\")"));
        }

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn exposes_build_annotation_control() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-annotations-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "/* @__PURE__ */ removable(); console.log('live')")
            .expect("write entry file");

        for (ignore_annotations, should_keep_call) in [(false, false), (true, true)] {
            let result = build(BuildOptions {
                entry_points: vec![entry.to_string_lossy().into_owned()],
                outdir: directory.join("out").to_string_lossy().into_owned(),
                ignore_annotations,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            let output = String::from_utf8_lossy(&result.output_files[0].contents);
            assert_eq!(output.contains("removable()"), should_keep_call);
            assert!(output.contains("console.log(\"live\")"));
        }

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn exposes_build_line_limits() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-line-limit-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(
            &entry,
            "console.log('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')",
        )
        .expect("write entry file");

        let build_with_limit = |line_limit| {
            build(BuildOptions {
                entry_points: vec![entry.to_string_lossy().into_owned()],
                outdir: directory.join("out").to_string_lossy().into_owned(),
                line_limit,
                ..BuildOptions::default()
            })
        };
        let unlimited = build_with_limit(0);
        let limited = build_with_limit(24);
        assert!(unlimited.errors.is_empty(), "{:?}", unlimited.errors);
        assert!(limited.errors.is_empty(), "{:?}", limited.errors);
        let unlimited = String::from_utf8_lossy(&unlimited.output_files[0].contents);
        let limited = String::from_utf8_lossy(&limited.output_files[0].contents);
        assert!(
            limited.lines().count() > unlimited.lines().count(),
            "{limited}"
        );
        assert!(limited.contains("console.log("));

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn applies_build_package_aliases() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-alias-{unique}"));
        std::fs::create_dir_all(directory.join("src")).expect("create source directory");
        std::fs::create_dir_all(directory.join("node_modules/replacement"))
            .expect("create package directory");
        std::fs::write(
            directory.join("src/entry.js"),
            "import { value } from 'original/feature'; console.log(value)",
        )
        .expect("write entry file");
        std::fs::write(
            directory.join("node_modules/replacement/package.json"),
            r#"{"main":"index.js","sideEffects":false}"#,
        )
        .expect("write package metadata");
        std::fs::write(
            directory.join("node_modules/replacement/feature.js"),
            "export const value = 'aliased package'",
        )
        .expect("write package module");

        let result = build(BuildOptions {
            entry_points: vec!["src/entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            alias: HashMap::from([("original".into(), "replacement".into())]),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("\"aliased package\""), "{output}");
        assert!(!output.contains("original/feature"));

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn applies_build_output_extensions() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-out-extension-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(directory.join("entry.js"), "console.log('js')")
            .expect("write JavaScript entry");
        std::fs::write(directory.join("style.css"), ".style { color: red }")
            .expect("write CSS entry");

        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into(), "style.css".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            out_extension: HashMap::from([
                (".js".into(), ".mjs".into()),
                (".css".into(), ".xcss".into()),
            ]),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let paths = result
            .output_files
            .iter()
            .map(|output| output.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|path| path.ends_with("/entry.mjs")));
        assert!(paths.iter().any(|path| path.ends_with("/style.xcss")));

        let invalid = build(BuildOptions {
            out_extension: HashMap::from([(".wat".into(), "invalid".into())]),
            ..BuildOptions::default()
        });
        assert!(!invalid.errors.is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn applies_css_build_banners_and_footers() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-css-banner-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(directory.join("entry.css"), ".entry { color: red }")
            .expect("write CSS entry");
        let result = build(BuildOptions {
            entry_points: vec!["entry.css".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            css_banner: "/* css before */".into(),
            css_footer: "/* css after */".into(),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.starts_with("/* css before */\n"));
        assert!(output.contains(".entry"));
        assert!(output.ends_with("/* css after */\n"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn applies_explicit_build_tsconfig() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-tsconfig-{unique}"));
        std::fs::create_dir_all(directory.join("config")).expect("create config directory");
        std::fs::create_dir_all(directory.join("src/lib")).expect("create source directory");
        std::fs::write(
            directory.join("config/tsconfig.json"),
            r#"{"compilerOptions":{"baseUrl":"..","paths":{"@lib/*":["src/lib/*"]}}}"#,
        )
        .expect("write tsconfig");
        std::fs::write(
            directory.join("src/entry.ts"),
            "import { value } from '@lib/value'; console.log(value)",
        )
        .expect("write entry");
        std::fs::write(
            directory.join("src/lib/value.ts"),
            "export const value: number = 123",
        )
        .expect("write dependency");

        let result = build(BuildOptions {
            entry_points: vec!["src/entry.ts".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            tsconfig: "config/tsconfig.json".into(),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("const value = 123;"), "{output}");
        assert!(output.contains("console.log(value);"), "{output}");

        let missing = build(BuildOptions {
            entry_points: vec!["src/entry.ts".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            tsconfig: "config/missing.json".into(),
            ..BuildOptions::default()
        });
        assert!(
            missing
                .errors
                .iter()
                .any(|error| error.text.contains("Cannot find tsconfig file"))
        );

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn applies_raw_tsconfig_to_builds_and_transforms() {
        let strict = transform(
            "console.log(123)",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"alwaysStrict":true}}"#.into(),
                ..TransformOptions::default()
            },
        );
        assert_eq!(code(strict), "\"use strict\";\nconsole.log(123);\n");

        let automatic_jsx = code(transform(
            "<><div /></>",
            TransformOptions {
                loader: Loader::Tsx,
                tsconfig_raw:
                    r#"{"compilerOptions":{"jsx":"react-jsx","jsxImportSource":"preact"}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            automatic_jsx.contains("from \"preact/jsx-runtime\""),
            "{automatic_jsx}"
        );
        assert!(
            automatic_jsx.contains("jsx(\"div\", {})"),
            "{automatic_jsx}"
        );

        let invalid = transform(
            "console.log(123)",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: "{".into(),
                ..TransformOptions::default()
            },
        );
        assert!(!invalid.errors.is_empty());
        assert!(invalid.code.is_empty());

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-tsconfig-raw-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("tsconfig.json"),
            r#"{"compilerOptions":{"jsxFactory":"nearest"}}"#,
        )
        .expect("write nearest tsconfig");
        std::fs::write(directory.join("entry.tsx"), "console.log(<div />)").expect("write entry");

        let result = build(BuildOptions {
            entry_points: vec!["entry.tsx".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            tsconfig_raw: r#"{"compilerOptions":{"jsxFactory":"raw"}}"#.into(),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("raw(\"div\", null)"), "{output}");
        assert!(!output.contains("nearest("), "{output}");

        let conflicting = build(BuildOptions {
            entry_points: vec!["entry.tsx".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            tsconfig: "tsconfig.json".into(),
            tsconfig_raw: "{}".into(),
            ..BuildOptions::default()
        });
        assert_eq!(
            conflicting
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Cannot provide \"tsconfig\" as both a raw string and a path")
        );

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn applies_raw_tsconfig_class_field_semantics() {
        let assignment_fields = code(transform(
            "class Foo { foo; static bar; static ready = createReady(); #private; [sideEffect()]; initialized = 1; ['quoted-key'] = 2; [3] = 4; 'quoted' = 5 }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(!assignment_fields.contains("foo;"), "{assignment_fields}");
        assert!(!assignment_fields.contains("bar;"), "{assignment_fields}");
        assert!(
            assignment_fields.contains("#private;"),
            "{assignment_fields}"
        );
        assert!(
            assignment_fields.contains("[sideEffect()];"),
            "{assignment_fields}"
        );
        assert!(
            assignment_fields.contains("this.initialized = 1;"),
            "{assignment_fields}"
        );
        assert!(
            !assignment_fields.contains("\n  initialized = 1;"),
            "{assignment_fields}"
        );
        assert!(
            assignment_fields.contains("this[\"quoted-key\"] = 2;"),
            "{assignment_fields}"
        );
        assert!(
            assignment_fields.contains("this[3] = 4;"),
            "{assignment_fields}"
        );
        assert!(
            assignment_fields.contains("this[\"quoted\"] = 5;"),
            "{assignment_fields}"
        );
        assert!(
            assignment_fields.contains("static {\n    this.ready = createReady();\n  }"),
            "{assignment_fields}"
        );

        let ordered = code(transform(
            "class Foo { initialized = 1; constructor(public id: string) { body() } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            ordered
                .find("this.initialized = 1")
                .expect("field assignment")
                < ordered.find("this.id = id").expect("parameter property"),
            "{ordered}"
        );

        let generated = code(transform(
            "class Foo { initialized = createValue() }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            generated.contains("constructor() {\n    this.initialized = createValue();"),
            "{generated}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lowers_only_capture_safe_type_script_class_fields() {
        let capture_sensitive = code(transform(
            "const outer = 1; class Foo { initialized = outer; constructor() { let outer = 2; use(outer) } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            capture_sensitive.contains("this.initialized = outer;"),
            "{capture_sensitive}"
        );
        assert!(
            capture_sensitive.contains("let outer2 = 2;"),
            "{capture_sensitive}"
        );
        assert!(
            capture_sensitive.contains("use(outer2);"),
            "{capture_sensitive}"
        );

        let parameter_capture = code(transform(
            "const id = 1; class Foo { initialized = id; constructor(id: number) { use(id) } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            parameter_capture.contains("this.initialized = id;"),
            "{parameter_capture}"
        );
        assert!(
            parameter_capture.contains("constructor(id2)"),
            "{parameter_capture}"
        );
        assert!(
            parameter_capture.contains("use(id2);"),
            "{parameter_capture}"
        );

        let nested_expressions = code(transform(
            "const outer = 1; class Foo { arrow = () => outer; functionValue = function() { return outer }; nested = class { method() { return outer } }; constructor() { let outer = 2; use(outer) } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            nested_expressions.contains("this.arrow = () => outer;"),
            "{nested_expressions}"
        );
        assert!(
            nested_expressions.contains("this.functionValue = function()"),
            "{nested_expressions}"
        );
        assert!(
            nested_expressions.contains("this.nested = class {"),
            "{nested_expressions}"
        );
        assert!(
            nested_expressions.contains("let outer2 = 2;")
                && nested_expressions.contains("use(outer2);"),
            "{nested_expressions}"
        );

        let preserved_jsx = code(transform(
            "const Component = Other; class Foo { node = <Component />; constructor() { let Component = Local; use(Component) } }",
            TransformOptions {
                loader: Loader::Tsx,
                jsx: BuildJsx::Preserve,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            preserved_jsx.contains("this.node = <Component />;"),
            "{preserved_jsx}"
        );
        assert!(
            preserved_jsx.contains("let Component2 = Local;")
                && preserved_jsx.contains("use(Component2);"),
            "{preserved_jsx}"
        );

        let non_colliding = code(transform(
            "class Foo { initialized = createValue(); constructor(id: number) { use(id) } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            non_colliding.contains("this.initialized = createValue();"),
            "{non_colliding}"
        );
        assert!(
            !non_colliding.contains("\n  initialized = createValue();"),
            "{non_colliding}"
        );

        let literal_template = code(transform(
            "class Foo { message = `hello`; constructor(id: number) { use(id) } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            literal_template.contains("this.message = \"hello\";"),
            "{literal_template}"
        );

        let define_fields = code(transform(
            "class Foo { foo }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":true}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(define_fields.contains("foo;"), "{define_fields}");
    }

    #[test]
    fn lowers_derived_type_script_class_fields() {
        let derived = code(transform(
            "class Foo extends Base { initialized = createValue(); constructor() { before(); super(); after() } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        let super_index = derived.find("super();").expect("super call");
        let field_index = derived
            .find("this.initialized = createValue();")
            .expect("field assignment");
        let after_index = derived.find("after();").expect("following statement");
        assert!(
            super_index < field_index && field_index < after_index,
            "{derived}"
        );

        let generated_derived = code(transform(
            "class Foo extends Base { initialized = createValue() }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            generated_derived.contains(
                "constructor() {\n    super(...arguments);\n    this.initialized = createValue();"
            ),
            "{generated_derived}"
        );

        let private = code(transform(
            "class Foo extends Base { #secret = 1; initialized = 2; constructor() { super() } }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(
            private.contains("super();\n    this.#secret = 1;\n    this.initialized = 2;"),
            "{private}"
        );
        assert!(
            private.find("constructor()").expect("constructor")
                < private.rfind("#secret;").expect("private declaration"),
            "{private}"
        );
    }

    #[test]
    fn rejects_invalid_build_loader_extensions() {
        let result = build(BuildOptions {
            loader: HashMap::from([("custom".into(), Loader::Text)]),
            ..BuildOptions::default()
        });
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].text.contains("Invalid file extension"));
    }

    #[test]
    fn transforms_type_script_to_javascript() {
        assert_eq!(
            code(transform(
                "const value: number = 1;",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "const value = 1;\n"
        );
    }

    #[test]
    fn preserves_comma_expressions_in_separator_contexts() {
        assert_eq!(
            code(transform(
                "foo((a,b)); const x = (a,b); const y = [(a,b)]; const {z = (a,b)} = obj;",
                TransformOptions::default()
            )),
            "foo((a, b));\n\
             const x = (a, b);\n\
             const y = [(a, b)];\n\
             const { z = (a, b) } = obj;\n"
        );
    }

    #[test]
    fn preserves_private_property_accesses() {
        assert_eq!(
            code(transform(
                "class Foo { #x; get() { return this.#x } set(value) { this.#x = value } }",
                TransformOptions::default()
            )),
            "class Foo {\n\
             \x20 #x;\n\
             \x20 get() {\n\
             \x20\x20\x20 return this.#x;\n\
             \x20 }\n\
             \x20 set(value) {\n\
             \x20\x20\x20 this.#x = value;\n\
             \x20 }\n\
             }\n"
        );
    }

    #[test]
    fn lowers_type_script_constructor_parameter_properties() {
        let defined = code(transform(
            "class Box { constructor(public id: string, readonly size = 1) {} }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert!(defined.contains("id;\n  size;"), "{defined}");
        assert!(defined.contains("this.id = id;"), "{defined}");
        assert!(defined.contains("this.size = size;"), "{defined}");
        assert!(
            defined.find("constructor(").expect("constructor")
                < defined.find("\n  id;\n").expect("synthetic field"),
            "{defined}"
        );

        let assigned = code(transform(
            "class Box { constructor(public id: string) {} }",
            TransformOptions {
                loader: Loader::Ts,
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        ));
        assert!(!assigned.contains("\n  id;"), "{assigned}");
        assert!(assigned.contains("this.id = id;"), "{assigned}");
    }

    #[test]
    fn initializes_type_script_parameter_properties_after_super() {
        let derived = code(transform(
            "class Box extends Base { constructor(public id: string) { before(); super(); after() } }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        let super_index = derived.find("super();").expect("super call");
        let assignment_index = derived.find("this.id = id;").expect("assignment");
        let after_index = derived.find("after();").expect("following statement");
        assert!(super_index < assignment_index && assignment_index < after_index);

        let conditional = code(transform(
            "class Box extends Base { constructor(public id: string) { if (flag) super(1); else { super(2) } } }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(
            conditional.matches("this.id = id;").count(),
            2,
            "{conditional}"
        );
        let first_super = conditional.find("super(1)").expect("first super call");
        let first_assignment = conditional.find("this.id = id;").expect("first assignment");
        let second_super = conditional.find("super(2);").expect("second super call");
        let second_assignment = conditional
            .rfind("this.id = id;")
            .expect("second assignment");
        assert!(first_super < first_assignment);
        assert!(second_super < second_assignment);

        let returned = code(transform(
            "class Box extends Base { constructor(public id: string) { return flag ? super(1) : (before(), super(2)) } }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(returned.matches("this.id = id").count(), 2, "{returned}");
        assert_eq!(
            returned.matches("this.id = id, this").count(),
            2,
            "{returned}"
        );

        let nested = code(transform(
            "class Box extends Base { constructor(public id: string) { return flag ? consume([super(1)]) : consume({ value: `${super(2)}` }) } }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(nested.matches("this.id = id").count(), 2, "{nested}");
        assert_eq!(nested.matches("this.id = id, this").count(), 2, "{nested}");
        assert!(
            nested.contains("consume([(super(1), this.id = id, this)])"),
            "{nested}"
        );
        assert!(
            nested.contains("${super(2), this.id = id, this}"),
            "{nested}"
        );

        let locals = code(transform(
            "class Box extends Base { constructor(public id: string) { if (flag) { const value = consume(super(1)); return value } else { const { value = super(2) } = source } } }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(locals.matches("this.id = id").count(), 2, "{locals}");
        assert_eq!(locals.matches("this.id = id, this").count(), 2, "{locals}");
        assert!(
            locals.contains("consume((super(1), this.id = id, this))"),
            "{locals}"
        );
        assert!(
            locals.contains("value = (super(2), this.id = id, this)"),
            "{locals}"
        );
    }

    #[test]
    fn initializes_type_script_parameter_properties_in_lexical_containers() {
        let arrow = code(transform(
            "class Box extends Base { constructor(public id: string) { const init = () => super(1); return init() } }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert!(arrow.contains("\n  id;"), "{arrow}");
        assert_eq!(arrow.matches("this.id = id").count(), 1, "{arrow}");
        assert!(
            arrow.contains("() => (super(1), this.id = id, this)"),
            "{arrow}"
        );

        let jsx = code(transform(
            "class Box extends Base { constructor(public id: string) { return <Widget value={super(1)}>{flag ? super(2) : null}</Widget> } }",
            TransformOptions {
                loader: Loader::Tsx,
                ..TransformOptions::default()
            },
        ));
        assert!(jsx.contains("\n  id;"), "{jsx}");
        assert_eq!(jsx.matches("this.id = id").count(), 2, "{jsx}");
        assert_eq!(jsx.matches("this.id = id, this").count(), 2, "{jsx}");
    }

    #[test]
    fn drops_debugger_statements_in_transforms_and_builds() {
        let transformed = code(transform(
            "debugger; function run() { debugger; return 1 }",
            TransformOptions {
                drop_debugger: true,
                ..TransformOptions::default()
            },
        ));
        assert!(!transformed.contains("debugger"));
        assert!(transformed.contains("function run()"));

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-drop-debugger-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "debugger; console.log('live'); debugger",
        )
        .expect("write entry file");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            drop_debugger: true,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(!output.contains("debugger"));
        assert!(output.contains("console.log(\"live\")"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn drops_console_calls_in_transforms_and_builds() {
        let dropped = code(transform(
            "
                console.log('foo')
                console.log(foo())
                console.log.call(console, foo())
                console.log.apply(console, foo())
                x = console.log(bar())
                console['log']('foo')
            ",
            TransformOptions {
                drop_console: true,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(dropped, "x = void 0;\n");

        let preserved = code(transform(
            "
                console('keep')
                console.abc.xyz('keep')
                console[abc][xyz]('keep')
                const bound = console.log.bind(console)
                function shadow(console) { console.log('keep') }
                if (ok) console.log('drop')
            ",
            TransformOptions {
                drop_console: true,
                ..TransformOptions::default()
            },
        ));
        assert!(preserved.contains("console(\"keep\")"));
        assert!(preserved.contains("}).xyz(\"keep\")"));
        assert!(preserved.contains("console[abc][xyz](\"keep\")"));
        assert!(preserved.contains("}).bind(console)"));
        assert!(preserved.contains("console2.log(\"keep\")"));
        assert!(preserved.contains("if (ok) ;"));
        assert!(!preserved.contains("\"drop\""));

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-drop-console-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "console.log(sideEffect()); keep()",
        )
        .expect("write entry file");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            drop_console: true,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(!output.contains("console"));
        assert!(!output.contains("sideEffect"));
        assert!(output.contains("keep();"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn drops_configured_labels_in_transforms_and_builds() {
        let transformed = code(transform(
            "DROP: { console.log('dead'); INNER: console.log('also dead') } KEEP: console.log('live')",
            TransformOptions {
                drop_labels: vec!["DROP".into()],
                ..TransformOptions::default()
            },
        ));
        assert!(!transformed.contains("dead"));
        assert!(!transformed.contains("DROP"));
        assert!(transformed.contains("KEEP:"));
        assert!(transformed.contains("console.log(\"live\")"));

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-drop-labels-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "DEV: console.log('development'); PROD: console.log('production')",
        )
        .expect("write entry file");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            drop_labels: vec!["DEV".into()],
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(!output.contains("development"));
        assert!(!output.contains("DEV:"));
        assert!(output.contains("PROD:"));
        assert!(output.contains("console.log(\"production\")"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn annotates_generated_jsx_calls_as_pure() {
        assert_eq!(
            code(transform(
                "const element = <div/>",
                TransformOptions {
                    loader: Loader::Jsx,
                    ..TransformOptions::default()
                }
            )),
            "const element = /* @__PURE__ */ React.createElement(\"div\", null);\n"
        );
        assert_eq!(
            code(transform(
                "const element = <div/>",
                TransformOptions {
                    loader: Loader::Jsx,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "const element=React.createElement(\"div\",null);\n"
        );
    }

    #[test]
    fn configures_jsx_transforms_and_side_effects() {
        let side_effectful = code(transform(
            "<Widget />",
            TransformOptions {
                loader: Loader::Jsx,
                jsx_factory: "h".into(),
                jsx_side_effects: true,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(side_effectful, "h(Widget, null);\n");

        let preserved = code(transform(
            "<Widget />",
            TransformOptions {
                loader: Loader::Jsx,
                jsx: BuildJsx::Preserve,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(preserved, "<Widget />;\n");

        let automatic = code(transform(
            "<Widget />",
            TransformOptions {
                loader: Loader::Jsx,
                jsx: BuildJsx::Automatic,
                jsx_import_source: "custom".into(),
                ..TransformOptions::default()
            },
        ));
        assert!(automatic.contains("from \"custom/jsx-runtime\""));
        assert!(automatic.contains("jsx(Widget, {})"));

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-jsx-side-effects-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.jsx"),
            "const dead = <Widget />; console.log('live')",
        )
        .expect("write entry file");
        let result = build(BuildOptions {
            entry_points: vec!["entry.jsx".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            jsx_side_effects: true,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("React.createElement(Widget, null)"));
        assert!(output.contains("console.log(\"live\")"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn preserves_user_authored_pure_annotations() {
        assert_eq!(
            code(transform(
                "const call = /* @__PURE__ */ factory();\
                 const instance = /* #__PURE__ */ new Factory();",
                TransformOptions {
                    loader: Loader::Js,
                    ..TransformOptions::default()
                }
            )),
            "const call = /* @__PURE__ */ factory();\n\
             const instance = /* @__PURE__ */ new Factory();\n"
        );
    }

    #[test]
    fn marks_configured_call_targets_as_pure() {
        let transformed = code(transform(
            "factory(); namespace.create(); other();",
            TransformOptions {
                pure: vec!["factory".into(), "namespace.create".into()],
                ..TransformOptions::default()
            },
        ));
        assert!(transformed.contains("/* @__PURE__ */ factory();"));
        assert!(transformed.contains("/* @__PURE__ */ namespace.create();"));
        assert!(transformed.contains("\nother();"));

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-pure-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "factory(); factory(sideEffect()); namespace.create(); other(); console.log('live')",
        )
        .expect("write entry file");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            pure: vec!["factory".into(), "namespace.create".into()],
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(!output.contains("factory"));
        assert!(!output.contains("namespace.create"));
        assert!(output.contains("sideEffect();"));
        assert!(output.contains("other();"));
        assert!(output.contains("console.log(\"live\")"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn keeps_function_and_class_names() {
        let transformed = code(transform(
            "var __name = 1; function LongFunction() {} class LongClass {} class Timed { static observed = Timed.name } class Shadowed { static name = 'custom' } const LongArrow = () => {}; const Different = function Inner() {}; const AnonymousShadowed = class { static name = 'custom' }; console.log(LongFunction.name, LongClass.name, Timed.observed, Shadowed.name, LongArrow.name, Different.name, AnonymousShadowed.name, __name)",
            TransformOptions {
                keep_names: true,
                ..TransformOptions::default()
            },
        ));
        assert!(transformed.starts_with(
            "var __defProp = Object.defineProperty;\n\
             var __name2 = (target, value) => __defProp(target, \"name\", { value, configurable: true });"
        ));
        assert!(transformed.contains("__name2(LongFunction, \"LongFunction\")"));
        assert!(transformed.contains("__name2(this, \"LongClass\")"));
        assert!(transformed.contains("__name2(() =>"));
        assert!(transformed.contains("\"LongArrow\")"));
        assert!(transformed.contains("__name2(function Inner()"));
        assert!(transformed.contains("\"Inner\")"));
        assert!(!transformed.contains("__name2(Shadowed"));
        assert!(!transformed.contains("\"AnonymousShadowed\")"));
        assert!(
            transformed
                .find("__name2(this, \"Timed\")")
                .expect("generated class name block")
                < transformed
                    .find("static observed")
                    .expect("user static initializer")
        );

        let colliding_helpers = code(transform(
            "var __defProp = 1; var __name = 2; const Foo = function() {};",
            TransformOptions {
                keep_names: true,
                ..TransformOptions::default()
            },
        ));
        assert!(colliding_helpers.starts_with(
            "var __defProp2 = Object.defineProperty;\n\
             var __name2 = (target, value) => __defProp2(target, \"name\", { value, configurable: true });"
        ));
        assert!(colliding_helpers.contains("__name2(function()"));
        assert!(colliding_helpers.contains("\"Foo\")"));

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-keep-names-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "function DeadFunction() {} function LongFunctionName() {} class LongClassName { static observed = LongClassName.name } const LongArrowName = () => {}; console.log(LongFunctionName.name, LongClassName.observed, LongArrowName.name)",
        )
        .expect("write entry file");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            minify_identifiers: true,
            keep_names: true,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("\"LongFunctionName\""));
        assert!(output.contains("\"LongClassName\""));
        assert!(output.contains("\"LongArrowName\""));
        assert!(!output.contains("DeadFunction"));
        let class_start = output.find("class ").expect("minified class declaration") + 6;
        let class_end = output[class_start..]
            .find(' ')
            .map(|offset| class_start + offset)
            .expect("class name terminator");
        let class_name = &output[class_start..class_end];
        assert!(output.contains(&format!("static observed = {class_name}.name;")));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn minifies_keep_name_helper_identifiers() {
        let basic = code(transform(
            "const Foo = function() {}; class Bar {}",
            TransformOptions {
                keep_names: true,
                minify_identifiers: true,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(
            basic,
            concat!(
                "var s = Object.defineProperty;\n",
                "var o = (c, n) => s(c, \"name\", { value: n, configurable: true });\n",
                "const Foo = /* @__PURE__ */ o(function() {\n",
                "}, \"Foo\");\n",
                "class Bar {\n",
                "  static {\n",
                "    o(this, \"Bar\");\n",
                "  }\n",
                "}\n",
            )
        );

        let competing_locals = code(transform(
            "function x(a,b,c,d){console.log(a,b,c,d)}",
            TransformOptions {
                keep_names: true,
                minify_identifiers: true,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(
            competing_locals,
            concat!(
                "var f = Object.defineProperty;\n",
                "var c = (o, n) => f(o, \"name\", { value: n, configurable: true });\n",
                "function x(o, n, l, e) {\n",
                "  console.log(o, n, l, e);\n",
                "}\n",
                "c(x, \"x\");\n",
            )
        );
    }

    #[test]
    fn keeps_inferred_names_across_expression_contexts() {
        let transformed = code(transform(
            "let assigned, pattern; assigned = function() {}; assigned ||= () => {};\
             ({ pattern = function() {} } = {});\
             const object = { 'field name': function() {}, method() {} };\
             class Fields { item = () => {}; static other = function() {}; static name = 'custom' }\
             function defaults(param = function() {}, [nested = () => {}] = []) {}\
             const [bound = function() {}] = [];\
             export default () => {};",
            TransformOptions {
                keep_names: true,
                ..TransformOptions::default()
            },
        ));
        for name in [
            "assigned",
            "pattern",
            "field name",
            "item",
            "other",
            "param",
            "nested",
            "bound",
            "default",
        ] {
            assert!(
                transformed.contains(&format!("\"{name}\"")),
                "missing inferred name {name:?} in {transformed}"
            );
        }
        assert!(!transformed.contains("\"method\""));
        assert!(!transformed.contains("__name(Fields"));
        assert!(!transformed.contains("from \"<runtime>\""));
        assert!(transformed.contains("({ pattern: pattern ="));

        let function = code(transform(
            "export default function() {}",
            TransformOptions {
                keep_names: true,
                ..TransformOptions::default()
            },
        ));
        assert!(function.contains("stdin_default"));
        assert!(function.contains("stdin_default, \"default\""));
        assert!(!function.contains("__name(default"));

        let class = code(transform(
            "export default class {}",
            TransformOptions {
                keep_names: true,
                ..TransformOptions::default()
            },
        ));
        assert!(class.contains("__name(this, \"default\")"));
        assert!(!class.contains("stdin_default"));

        let invalid = transform(
            "({ invalid = function() {} });",
            TransformOptions::default(),
        );
        assert_eq!(invalid.errors.len(), 1);
        assert_eq!(invalid.errors[0].text, "Unexpected \"=\"");
        let location = invalid.errors[0].location.as_ref().expect("error location");
        assert_eq!(location.file, "<stdin>");
        assert_eq!(location.line, 1);
        assert_eq!(location.column, 13);
        assert_eq!(location.length, 0);
        assert_eq!(location.line_text, "({ invalid = function() {} });");
    }

    #[test]
    fn transforms_type_script_enums_through_lowering() {
        assert_eq!(
            code(transform(
                "enum Color { Red, Blue = 'blue' } const red = Color.Red;",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "var Color = /* @__PURE__ */ ((Color2) => {\n\
             \x20\x20Color2[Color2[\"Red\"] = 0] = \"Red\";\n\
             \x20\x20Color2[\"Blue\"] = \"blue\";\n\
             \x20\x20return Color2;\n\
             })(Color || {});\n\
             const red = 0 /* Red */;\n"
        );
        let impure = code(transform(
            "enum Value { Item = sideEffect() }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert!(impure.contains("var Value = ((Value2) =>"));
        assert!(!impure.contains("@__PURE__"));

        let merged = code(transform(
            "enum Foo { A = 1 } enum Foo { B = 2 }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(merged.matches("var Foo =").count(), 2, "{merged}");

        let nested = code(transform(
            "namespace N { export enum E { A = 1 } export enum E { B = 2 } }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert_eq!(
            nested,
            "var N;\n((N2) => {\n  let E;\n  ((E2) => {\n    E2[E2[\"A\"] = 1] = \"A\";\n  })(E = N2.E || (N2.E = {}));\n  ((E2) => {\n    E2[E2[\"B\"] = 2] = \"B\";\n  })(E = N2.E || (N2.E = {}));\n})(N || (N = {}));\n"
        );
        assert_eq!(
            code(transform(
                "namespace N { export enum E { A = 1 } export enum E { B = 2 } }",
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "var N;(n=>{let m;(e=>e[e.A=1]=\"A\")(m=n.E||={}),(e=>e[e.B=2]=\"B\")(m=n.E||={})})(N||={});\n"
        );
    }

    #[test]
    fn lowers_nested_typescript_enums_like_esbuild() {
        assert_eq!(
            code(transform(
                "{ enum E { A = 1 } }",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "{\n  let E;\n  ((E2) => {\n    E2[E2[\"A\"] = 1] = \"A\";\n  })(E || (E = {}));\n}\n"
        );
        let function = "function f(){enum E{A};return E}";
        assert_eq!(
            code(transform(
                function,
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "function f() {\n  let E;\n  return ((E2) => E2[E2.A = 0] = \"A\")(E ||= {}), E;\n}\n"
        );
        assert_eq!(
            code(transform(
                function,
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "function f(){let n;return(u=>u[u.A=0]=\"A\")(n||={}),n}\n"
        );
        assert_eq!(
            code(transform(
                "const f=()=>{enum E{A,B};return E}",
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "const f=()=>{let e;return(n=>(n[n.A=0]=\"A\",n[n.B=1]=\"B\"))(e||={}),e};\n"
        );
    }

    #[test]
    fn rejects_typescript_namespaces_outside_module_or_namespace_scopes() {
        for input in [
            "{namespace N{}}",
            "if(x){namespace N{}}",
            "function f(){namespace N{}}",
            "(()=>{namespace N{}})()",
        ] {
            let result = transform(
                input,
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                },
            );
            assert_eq!(result.errors.len(), 1, "{input}: {:?}", result.errors);
            assert_eq!(result.errors[0].text, "Expected \";\" but found \"N\"");
        }
    }

    #[test]
    fn preserves_empty_statements_after_type_script_interfaces() {
        assert_eq!(
            code(transform(
                "interface X { x: number }; const x: X = { x: 1 }",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            ";\nconst x = { x: 1 };\n"
        );
        assert_eq!(
            code(transform(
                "interface X { x: number } const x: X = { x: 1 }",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "const x = { x: 1 };\n"
        );
    }

    #[test]
    fn numbers_type_script_namespace_scope_collisions() {
        assert_eq!(
            code(transform(
                "namespace N { export const x = 1 } console.log(N.x)",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "var N;\n\
             ((N2) => {\n\
             \x20\x20N2.x = 1;\n\
             })(N || (N = {}));\n\
             console.log(N.x);\n"
        );
    }

    #[test]
    fn transforms_and_minifies_css() {
        assert_eq!(
            code(transform(
                ".card { color: red; margin: 0 !important }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".card{color:red;margin:0!important}\n"
        );
    }

    #[test]
    fn minifies_core_css_syntax() {
        assert_eq!(
            code(transform(
                ".a {} .b { color: blue }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".b{color:#00f}\n"
        );
        assert_eq!(
            code(transform(
                "@keyframes fade { from { opacity: 0 } to { opacity: 1 } } .fade { animation: fade 1s }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "@keyframes fade{0%{opacity:0}to{opacity:1}}.fade{animation:fade 1s}\n"
        );
        assert_eq!(
            code(transform(
                ".a { width: calc(1px + 2px); height: calc(2 * 3px); opacity: calc(1 / 2); margin: calc(2px * 3 + 4px * 5); padding: calc((2px + 3px) * 4); inset: calc(100% / 8); top: calc(100% / 3) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".a{width:3px;height:6px;opacity:.5;margin:26px;padding:20px;inset:12.5%;top:calc(100% / 3)}\n"
        );
    }

    #[test]
    fn minifies_symbolic_calc_expressions() {
        assert_eq!(
            code(transform(
                "a { one: calc(x + -1); two: calc(x - -1); three: calc(1px - x + 2px); four: calc(1px - var(x) + 2px); five: calc(x * .25); six: calc(x / .25); seven: calc((a + b) + c); eight: calc(a + (b + c)); nine: calc(2px * 3 + x); ten: calc(x * var(y)) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{one:calc(x - 1);two:calc(x + 1);three:calc(3px - x);four:calc(1px - var(x) + 2px);five:calc(x/4);six:calc(x*4);seven:calc(a + b + c);eight:calc(a + b + c);nine:calc(6px + x);ten:calc(x * var(y))}\n"
        );
    }

    #[test]
    fn minifies_grouped_calc_expressions() {
        assert_eq!(
            code(transform(
                "a { one: calc((a * b) * c); two: calc(a * (b * c)); three: calc((a / b) / c); four: calc(a / (b / c)); five: calc(a * (b / c)); six: calc(a / (b * c)); seven: calc(3 * (2px + 1em / 8)); eight: calc(3 * (2px + 1em / 7)); nine: calc((2px * 3) + x) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{one:calc(a*b*c);two:calc(a*b*c);three:calc(a/b/c);four:calc(a/(b/c));five:calc(a*b/c);six:calc(a/(b*c));seven:calc(3*(2px + .125em));eight:calc(3 * (2px + 1em / 7));nine:calc(6px + x)}\n"
        );
    }

    #[test]
    fn minifies_css_declarations() {
        assert_eq!(
            code(transform(
                "a { padding: 1px 1px 1px 1px; margin: 1px 2px 1px 2px; inset: 1px 2px 1px; color: rgb(300, 0, 0); background: rgba(0, 0, 255, 1); background-color: #FFFFFFFF; outline-color: #ff0000; caret-color: rgb(255, 0, 0); font-weight: normal; opacity: 0.5000; border-color: red red red red; scroll-margin: 1px 2px 1px 2px } b { color: rgba(255, 0, 0, 50%); background: rgb(50%, 25%, 0%) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{padding:1px;margin:1px 2px;inset:1px 2px;color:red;background:#00f;background-color:#fff;outline-color:red;caret-color:red;font-weight:400;opacity:.5;border-color:red red red red;scroll-margin:1px 2px 1px 2px}b{color:#ff00007f;background:#7f4000}\n"
        );
    }

    #[test]
    fn merges_css_box_declarations() {
        assert_eq!(
            code(transform(
                "a { margin: 1px 2px 3px 4px; margin-top: 5px } b { padding: 1px 2px; padding-top: 5px } c { inset: 1px; top: 5px } d { margin-left: 1px; margin-right: 2px; margin-top: 3px; margin-bottom: 4px } e { padding: 1px 2px 3px 4px; padding-left: -4px; padding-right: -2px } f { margin: var(--x) var(--y) var(--z) var(--y) } g { margin: 1px auto 3px 4px; margin-left: auto } h { inset: auto; left: 1px } i { padding: 1px auto 3px 4px; padding-left: auto } j { margin-left: 1Q; margin-right: 2Q; margin-top: 3Q; margin-bottom: 0 } k { margin: 1px; margin-top: 2px !important }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{margin:5px 2px 3px 4px}b{padding:5px 2px 1px}c{inset:5px 1px 1px}d{margin:3px 2px 4px 1px}e{padding:1px -2px 3px -4px}f{margin:var(--x) var(--y) var(--z) var(--y)}g{margin:1px auto 3px}h{inset:auto auto auto 1px}i{padding:1px auto 3px 4px;padding-left:auto}j{margin-left:1Q;margin-right:2Q;margin-top:3Q;margin-bottom:0}k{margin:1px;margin-top:2px!important}\n"
        );
    }

    #[test]
    fn minifies_css_time_dimensions() {
        assert_eq!(
            code(transform(
                "a { a: .001s; b: .0012s; c: -.001s; d: .000123s; e: .001S; f: 100ms; g: 120ms; h: 123ms; i: 1000ms; j: 1200ms; k: 1230ms; l: 1234ms; m: -100ms; n: 120mS; o: 123mS; p: 1e3ms }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{a:1ms;b:1.2ms;c:-1ms;d:.123ms;e:1ms;f:.1s;g:.12s;h:123ms;i:1s;j:1.2s;k:1.23s;l:1234ms;m:-.1s;n:.12s;o:123mS;p:1e3ms}\n"
        );
    }

    #[test]
    fn minifies_css_font_declarations() {
        assert_eq!(
            code(transform(
                "a { font-family: 'serif' } b { font-family: 'aaa bbb', serif } c { font-family: 'aaa  bbb', serif } d { font-family: 'initial', serif } e { font-family: 'revert-layer', 'Segoe UI', serif } f { font: 1rem 'aaa bbb' } g { font: 1rem / 1.2 'aaa bbb' } h { font: normal 1rem 'aaa bbb' } i { font: italic small-caps bold ultra-condensed 1rem / 1.2 'aaa bbb' } j { font: oblique 45deg 1px 'aaa bbb' } k { font: var(--var) 'aaa bbb' } l { font: 10px '123' }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{font-family:\"serif\"}b{font-family:aaa bbb,serif}c{font-family:\"aaa  bbb\",serif}d{font-family:\"initial\",serif}e{font-family:\"revert-layer\",Segoe UI,serif}f{font:1rem aaa bbb}g{font:1rem/1.2 aaa bbb}h{font: 1rem aaa bbb}i{font:italic small-caps 700 ultra-condensed 1rem/1.2 aaa bbb}j{font:oblique 45deg 1px aaa bbb}k{font:var(--var) \"aaa bbb\"}l{font:10px \"123\"}\n"
        );
        assert_eq!(
            code(transform(
                "a { font-family: 'revert-layer','Segoe UI',serif }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "a {\n  font-family:\n    \"revert-layer\",\n    Segoe UI,\n    serif;\n}\n"
        );
    }

    #[test]
    fn processes_local_css_list_style_names() {
        assert_eq!(
            code(transform(
                "div { list-style-type: custom } div { list-style: custom none } div { list-style: none custom } div { list-style: custom inside } div { list-style: inside inside } div { list-style-type: decimal } div { list-style: INITIAL }",
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    minify_syntax: true,
                    minify_whitespace: true,
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "div{list-style-type:i}div{list-style:i none}div{list-style:none i}div{list-style:i inside}div{list-style:inside s}div{list-style-type:decimal}div{list-style:INITIAL}\n"
        );
    }

    #[test]
    fn links_local_counter_style_definitions_and_references() {
        assert_eq!(
            code(transform(
                "@counter-style custom { system: fixed; symbols: \"x\" } a { list-style-type: custom } @counter-style second { system: cyclic; symbols: \"y\" } b { list-style: second inside }",
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    minify_syntax: true,
                    minify_whitespace: true,
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "@counter-style s{system:fixed;symbols:\"x\"}a{list-style-type:s}@counter-style e{system:cyclic;symbols:\"y\"}b{list-style:e inside}\n"
        );
    }

    #[test]
    fn processes_local_css_container_names() {
        assert_eq!(
            code(transform(
                "div { container-name: NONE initial } div { container-name: local1 local2 } div { container: none } div { container: NONE / size } div { container: local1 local2 } div { container: local1 local2 / size } div { container: local1 / size extra } div { container-name: local1 / size }",
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    minify_syntax: true,
                    minify_whitespace: true,
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "div{container-name:NONE initial}div{container-name:i n}div{container:none}div{container:NONE / size}div{container:i n}div{container:i n / size}div{container:local1 / size extra}div{container-name:local1 / size}\n"
        );
    }

    #[test]
    fn links_local_container_queries_and_names() {
        assert_eq!(
            code(transform(
                "a { container-name: foo bar } @container foo (width > 1px) { b { color: red } } @container bar style(--x: true) { c { color: blue } } @container not (width > 1px) { d { color: black } }",
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    minify_syntax: true,
                    minify_whitespace: true,
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            "a{container-name:o n}@container o (width > 1px){b{color:red}}@container n style(--x: true){c{color:#00f}}@container not (width > 1px){d{color:#000}}\n"
        );
    }

    #[test]
    fn minifies_css_transforms() {
        assert_eq!(
            code(transform(
                "a { transform: translate(0px, 0em) translate(0px, 2px) translateX(0%) scale(2, 2) scale(2, 1) scale(1, 3) scale(50%) rotateZ(0deg) skewX(0rad) skew(2deg, 0turn) matrix(2, 0, 0, 2, 0, 0) translate3d(0px, 0%, 2px) scale3d(1, 1, 50%) rotate3d(1, 0, 0, 0deg) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{transform:translate(0) translateY(2px) translate(0) scale(2) scaleX(2) scaleY(3) scale(.5) rotate(0) skew(0) skew(2deg) scale(2) translateZ(2px) scaleZ(.5) rotateX(0)}\n"
        );
    }

    #[test]
    fn minifies_named_css_colors() {
        assert_eq!(
            code(transform(
                "a { color: aliceblue; background-color: rebeccapurple; outline-color: darkslategrey; caret-color: navy; fill: fuchsia; stroke: transparent }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{color:#f0f8ff;background-color:#639;outline-color:#2f4f4f;caret-color:navy;fill:#f0f;stroke:transparent}\n"
        );
    }

    #[test]
    fn minifies_modern_css_color_functions() {
        assert_eq!(
            code(transform(
                "a { color: rgb(1 2 3) } b { color: rgba(1 2 3 / .5) } c { color: rgb(1% 2% 3% / 50%) } d { color: hsl(0, 100%, 50%) } e { color: hsl(30deg, 100%, 50%) } f { color: hsl(60 100% 50%) } g { color: hsl(200grad, 100%, 50%) } h { color: hsl(.75turn 100% 50%) } i { color: hsl(30 25% 50% / 50%) } j { color: hwb(90deg 20% 40%) } k { color: hwb(.75turn 20% 40% / .75) } l { color: hwb(1deg 40% 80%) } m { color: hwb(90deg, 20%, 40%) } n { color: hsl(var(--x) var(--y) var(--z)) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{color:#010203}b{color:#01020380}c{color:#0305087f}d{color:red}e{color:#ff8000}f{color:#ff0}g{color:#0ff}h{color:#7f00ff}i{color:#9f80607f}j{color:#693}k{color:#663399bf}l{color:#555}m{color:hwb(90deg,20%,40%)}n{color:hsl(var(--x) var(--y) var(--z))}\n"
        );
    }

    #[test]
    fn minifies_css_gradient_syntax() {
        assert_eq!(
            code(transform(
                "a { background: linear-gradient(yellow, #11223344) } b { background-image: radial-gradient(yellow 10%, #11223344 90%) } c { border-image: conic-gradient(yellow, 25%, #11223344) } d { mask-image: repeating-linear-gradient(green, red 10%, red 20%, yellow 70% 80%, black) } e { background: repeating-radial-gradient(red 0%, green 25%, blue 50%, white 100%) } f { background: repeating-conic-gradient(red 0deg, green 90deg, blue 180deg, white 1turn) } g { background: linear-gradient(to right, red 0%, green 50%, blue 100%) } h { background: linear-gradient(var(--stops)) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{background:linear-gradient(#ff0,#1234)}b{background-image:radial-gradient(#ff0 10%,#1234 90%)}c{border-image:conic-gradient(#ff0,25%,#1234)}d{mask-image:repeating-linear-gradient(green,red 10% 20%,#ff0 70% 80%,#000)}e{background:repeating-radial-gradient(red,green,#00f 50%,#fff)}f{background:repeating-conic-gradient(red,green,#00f 180deg,#fff 1turn)}g{background:linear-gradient(to right,red,green,#00f)}h{background:linear-gradient(var(--stops))}\n"
        );
        assert_eq!(
            code(transform(
                "a { background: linear-gradient(to right, red 0%, green 50%, blue 100%) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "a {\n  background:\n    linear-gradient(\n      to right,\n      red,\n      green,\n      #00f);\n}\n"
        );
    }

    #[test]
    fn merges_safe_adjacent_css_selector_rules() {
        assert_eq!(
            code(transform(
                ".a { color: red } .b { color: red } .b { color: red } a:focus { color: red } b:focus { color: red } div { margin: 0 } span { margin: 0 } article { padding: 0 } section { padding: 0 }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".a,.b{color:red}a:focus{color:red}b:focus{color:red}div,span{margin:0}article{padding:0}section{padding:0}\n"
        );
    }

    #[test]
    fn preserves_legal_css_comments() {
        assert_eq!(
            code(transform(
                "/*! first */ .a { color: red } /*! middle */ .b { color: red } a { /*! dropped */ color: blue } @media print { /*! kept */ c { color: black } } /* @license last */",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "/*! first */.a,.b{color:red}/*! middle */a{color:#00f}@media print{/*! kept */c{color:#000}}/* @license last */\n"
        );
    }

    #[test]
    fn mangles_empty_and_nested_css_at_rules() {
        assert_eq!(
            code(transform(
                "@media screen {} @supports (display: grid) {} @layer foo {} @layer {} @layer a { @layer b { c { color: red } } } @keyframes x {} @container x {} @starting-style {} @font-face {} @page {}",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "@layer foo;@layer{}@layer a.b{c{color:red}}@keyframes x{}@starting-style{}\n"
        );
    }

    #[test]
    fn unwraps_duplicate_nested_css_media_rules() {
        assert_eq!(
            code(transform(
                "@media screen { a { color: red } @media screen { b { color: blue } } @media print { c { color: black } } } @media (min-width: 1px) { @media (min-width: 1px) { d { color: white } } }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "@media screen{a{color:red}b{color:#00f}@media print{c{color:#000}}}@media(min-width:1px){d{color:#fff}}\n"
        );
    }

    #[test]
    fn minifies_css_box_shadows() {
        assert_eq!(
            code(transform(
                "a { box-shadow: 0px 0em 0rem 0cm black, inset 0px 1px 0px 0px rgb(255, 0, 0), 1px 2px 3px 4px aliceblue } b { box-shadow: var(--x) 0px 0px 0px black } c { text-shadow: 0px 0px 0px blue }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{box-shadow:0 0 #000,inset 0 1px red,1px 2px 3px 4px #f0f8ff}b{box-shadow:var(--x) 0 0 0 #000}c{text-shadow:0px 0px 0px blue}\n"
        );
    }

    #[test]
    fn minifies_css_border_radii() {
        assert_eq!(
            code(transform(
                "a { border-radius: 1px 1px 1px 1px; one: 1px } b { border-radius: 1px 2px 1px 2px } c { border-radius: 1px 2px 3px 2px } d { border-radius: 1px 2px / 1px 2px } e { border-radius: 1px 2px 3px 2px / 4px 5px 4px 5px } f { border-top-left-radius: 2px 2px; border-bottom-right-radius: 2px 3px } g { border-radius: var(--x) var(--y) var(--z) var(--y) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{border-radius:1px;one:1px}b{border-radius:1px 2px}c{border-radius:1px 2px 3px}d{border-radius:1px 2px}e{border-radius:1px 2px 3px/4px 5px}f{border-top-left-radius:2px;border-bottom-right-radius:2px 3px}g{border-radius:var(--x) var(--y) var(--z) var(--y)}\n"
        );
    }

    #[test]
    fn merges_css_border_radius_declarations() {
        assert_eq!(
            code(transform(
                "a { border-top-left-radius: 0 0px } b { border-radius: 1px 2px; border-top-left-radius: 3px } c { border-radius: 0 / 1px 2px; border-top-left-radius: 3px } d { border-radius: 1px 2px 3px 4px; border-top-right-radius: 5px 6px } e { border-radius: 1px; border-top-left-radius: 2px !important } f { border-radius: 1px !important; border-top-left-radius: 2px } g { border-radius: 1rem; border-top-left-radius: 1vw } h { border-radius: 0; border-top-left-radius: 2rem } i { border-top-left-radius: 1px; border-radius: 2px } j { border-radius: 1px; border-radius: 2px }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{border-top-left-radius:0}b{border-radius:3px 2px 1px}c{border-radius:3px 0 0/3px 2px 1px}d{border-radius:1px 5px 3px 4px/1px 6px 3px 4px}e{border-radius:1px;border-top-left-radius:2px!important}f{border-radius:1px!important;border-top-left-radius:2px}g{border-radius:1rem;border-top-left-radius:1vw}h{border-radius:0;border-top-left-radius:2rem}i{border-radius:2px}j{border-radius:2px}\n"
        );
    }

    #[test]
    fn scopes_and_minifies_local_css_names() {
        let input = ".card { color: red } #root .card { color: blue }";
        assert_eq!(
            code(transform(
                input,
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    ..TransformOptions::default()
                }
            )),
            ".entry_card {\n\
             \x20\x20color: red;\n\
             }\n\
             #entry_root .entry_card {\n\
             \x20\x20color: blue;\n\
             }\n"
        );
        assert_eq!(
            code(transform(
                input,
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            ".o {\n\
             \x20\x20color: red;\n\
             }\n\
             #l .o {\n\
             \x20\x20color: blue;\n\
             }\n"
        );
    }

    #[test]
    fn scopes_local_css_animation_and_explicit_selector_modes() {
        assert_eq!(
            code(transform(
                "@keyframes fade { from { opacity: 0 } }\
                 .fade { animation: fade 1s }\
                 :global(.external) .local, :local(.forced) {}",
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    ..TransformOptions::default()
                }
            )),
            "@keyframes entry_fade {\n\
             \x20\x20from {\n\
             \x20\x20\x20\x20opacity: 0;\n\
             \x20\x20}\n\
             }\n\
             .entry_fade {\n\
             \x20\x20animation: entry_fade 1s;\n\
             }\n\
             .external .entry_local,\n\
             .entry_forced {\n\
             }\n"
        );
    }

    #[test]
    fn transforms_json_into_a_common_js_export() {
        assert_eq!(
            code(transform(
                r#"{"answer": 42, "invalid-identifier": true}"#,
                TransformOptions {
                    loader: Loader::Json,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = { answer: 42, \"invalid-identifier\": true };\n"
        );
    }

    #[test]
    fn transforms_text_base64_and_binary_loaders() {
        assert_eq!(
            code(transform(
                b"\xef\xbb\xbfhello",
                TransformOptions {
                    loader: Loader::Text,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"hello\";\n"
        );
        assert_eq!(
            code(transform(
                "hello",
                TransformOptions {
                    loader: Loader::Base64,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"aGVsbG8=\";\n"
        );
        assert_eq!(
            code(transform(
                "hello",
                TransformOptions {
                    loader: Loader::Binary,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = Uint8Array.fromBase64(\"aGVsbG8=\");\n"
        );
    }

    #[test]
    fn transforms_data_urls_and_empty_input() {
        assert_eq!(
            code(transform(
                "<svg></svg>",
                TransformOptions {
                    sourcefile: "icon.svg".into(),
                    loader: Loader::DataUrl,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"data:image/svg+xml,<svg></svg>\";\n"
        );
        assert_eq!(
            code(transform(
                [0xff],
                TransformOptions {
                    loader: Loader::DataUrl,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"data:application/octet-stream;base64,/w==\";\n"
        );
        assert_eq!(
            code(transform(
                "ignored",
                TransformOptions {
                    loader: Loader::Empty,
                    ..TransformOptions::default()
                }
            )),
            ""
        );
    }

    #[test]
    fn minifies_data_loader_exports() {
        assert_eq!(
            code(transform(
                r#"{"x": 1}"#,
                TransformOptions {
                    loader: Loader::Json,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "module.exports={x:1};\n"
        );
    }

    #[test]
    fn default_loader_uses_the_sourcefile_extension() {
        assert_eq!(
            code(transform(
                "const value: number = 1",
                TransformOptions {
                    sourcefile: "entry.ts".into(),
                    loader: Loader::Default,
                    ..TransformOptions::default()
                }
            )),
            "const value = 1;\n"
        );
        assert_eq!(
            code(transform(
                r#"{"x": 1}"#,
                TransformOptions {
                    sourcefile: "entry.json".into(),
                    loader: Loader::Default,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = { x: 1 };\n"
        );
        let result = transform(
            "data",
            TransformOptions {
                sourcefile: "entry.unknown".into(),
                loader: Loader::Default,
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            result.errors.first().map(|message| message.text.as_str()),
            Some("Do not know how to load path: entry.unknown")
        );
    }

    #[test]
    fn returns_diagnostics_and_suppresses_code_on_errors() {
        let result = transform(
            "const = 1",
            TransformOptions {
                loader: Loader::Js,
                ..TransformOptions::default()
            },
        );
        assert!(!result.errors.is_empty());
        assert!(result.code.is_empty());
    }

    #[test]
    fn adds_transform_banners_and_footers() {
        assert_eq!(
            code(transform(
                "let x = 1",
                TransformOptions {
                    banner: "/* before */".into(),
                    footer: "/* after */".into(),
                    ..TransformOptions::default()
                }
            )),
            "/* before */\nlet x = 1;\n/* after */\n"
        );
    }

    #[test]
    fn configures_transform_legal_comments_for_javascript_and_css() {
        for (loader, input, output_without_comment, inline_output, legal_comment) in [
            (Loader::Js, "//!x\ny()", "y();\n", "//!x\ny();\n", "//!x\n"),
            (
                Loader::Css,
                "/*!x*/\ny{}",
                "y {\n}\n",
                "/*!x*/\ny {\n}\n",
                "/*!x*/\n",
            ),
        ] {
            let transformed = |legal_comments| {
                transform(
                    input,
                    TransformOptions {
                        loader,
                        legal_comments,
                        ..TransformOptions::default()
                    },
                )
            };

            assert_eq!(
                code(transformed(BuildLegalComments::None)),
                output_without_comment
            );
            assert_eq!(code(transformed(BuildLegalComments::Inline)), inline_output);

            let eof = transformed(BuildLegalComments::EndOfFile);
            assert!(eof.errors.is_empty(), "{:?}", eof.errors);
            assert_eq!(
                String::from_utf8(eof.code).expect("transform output is UTF-8"),
                format!("{output_without_comment}{legal_comment}")
            );
            assert!(eof.legal_comments.is_empty());

            let external = transformed(BuildLegalComments::External);
            assert!(external.errors.is_empty(), "{:?}", external.errors);
            assert_eq!(
                String::from_utf8(external.code).expect("transform output is UTF-8"),
                output_without_comment
            );
            assert_eq!(
                String::from_utf8(external.legal_comments)
                    .expect("external legal comments are UTF-8"),
                legal_comment
            );
        }

        let linked = transform(
            "",
            TransformOptions {
                legal_comments: BuildLegalComments::Linked,
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            linked.errors.first().map(|message| message.text.as_str()),
            Some("Cannot transform with linked legal comments")
        );
        assert!(linked.code.is_empty());

        let escaped = transform(
            "/*! </script> */\nkeep()",
            TransformOptions {
                legal_comments: BuildLegalComments::EndOfFile,
                footer: "footer()".into(),
                ..TransformOptions::default()
            },
        );
        assert_eq!(code(escaped), "keep();\n/*! <\\/script> */\nfooter()\n");
    }

    #[test]
    fn defaults_legal_comments_by_api_context() {
        assert_eq!(
            code(transform(
                "/*! transform license */\nkeep()",
                TransformOptions::default()
            )),
            "/*! transform license */\nkeep();\n"
        );

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("esbuild-rs-default-legal-comments-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.js"),
            "/*! build license */\nconsole.log('live')",
        )
        .expect("write entry file");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        let code_index = output.find("console.log").expect("build code");
        let comment_index = output.find("/*! build license */").expect("legal comment");
        assert!(comment_index > code_index, "{output}");
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn generates_transform_source_maps() {
        let missing_sourcefile = transform(
            "1+2",
            TransformOptions {
                sourcemap: BuildSourceMap::External,
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            missing_sourcefile
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Must use \"sourcefile\" with \"sourcemap\" to set the original file name")
        );
        assert!(missing_sourcefile.code.is_empty());
        assert!(missing_sourcefile.map.is_empty());

        let configured = transform(
            "let       x",
            TransformOptions {
                sourcefile: "afile.js".into(),
                sourcemap: BuildSourceMap::External,
                source_root: "https://example.com/".into(),
                sources_content: BuildSourcesContent::Exclude,
                banner: "/* banner */".into(),
                ..TransformOptions::default()
            },
        );
        assert!(configured.errors.is_empty(), "{:?}", configured.errors);
        let configured_map = String::from_utf8(configured.map).expect("source map is UTF-8");
        assert!(configured_map.contains("\"sources\": [\"afile.js\"]"));
        assert!(configured_map.contains("\"sourceRoot\": \"https://example.com/\""));
        assert!(!configured_map.contains("\"sourcesContent\""));
        assert!(configured_map.contains("\"mappings\": \";AAAA"));

        let inline = transform(
            "1+2",
            TransformOptions {
                sourcefile: "inline.js".into(),
                sourcemap: BuildSourceMap::Inline,
                ..TransformOptions::default()
            },
        );
        assert!(inline.errors.is_empty(), "{:?}", inline.errors);
        assert!(inline.map.is_empty());
        assert!(
            String::from_utf8(inline.code)
                .expect("transform output is UTF-8")
                .starts_with("1 + 2;\n//# sourceMappingURL=data:application/json;base64,")
        );

        let both = transform(
            "a{b:c}",
            TransformOptions {
                sourcefile: "style.css".into(),
                loader: Loader::Css,
                sourcemap: BuildSourceMap::InlineAndExternal,
                ..TransformOptions::default()
            },
        );
        assert!(both.errors.is_empty(), "{:?}", both.errors);
        assert!(!both.map.is_empty());
        assert!(
            String::from_utf8(both.code)
                .expect("transform output is UTF-8")
                .contains("/*# sourceMappingURL=data:application/json;base64,")
        );

        let linked = transform(
            "1+2",
            TransformOptions {
                sourcemap: BuildSourceMap::Linked,
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            linked.errors.first().map(|message| message.text.as_str()),
            Some("Cannot transform with linked source maps")
        );
        assert!(linked.code.is_empty());
        assert!(linked.map.is_empty());
    }
}
