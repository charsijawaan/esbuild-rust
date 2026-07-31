//! Port of esbuild's public `pkg/api` package.

mod watcher;

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    fmt::{self, Write as _},
    fs as std_fs,
    io::{self, Write as _},
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Condvar, Mutex, RwLock, Weak},
};

use crate::internal::{
    ast::{
        DEFAULT_NAME_MINIFIER_CSS, DEFAULT_NAME_MINIFIER_JS, ImportKind, Ref, SymbolKind, SymbolMap,
    },
    bundler,
    cache::CacheSet,
    config::{self, Mode},
    css_parser, css_printer,
    fs::{Fs, MockKind, RealFsOptions, WatchData, mock_fs, real_fs},
    helpers::{
        encode_string_as_shortest_data_url, escape_closing_tag, mime_type_by_extension,
        quote_for_json, string_to_utf16,
    },
    js_ast::generate_non_unique_name_from_path,
    js_parser, js_printer,
    logger::{
        DeferLogKind, Log, Msg, MsgData, MsgKind, MsgLocation, OutputOptions, Path, PathStyle,
        PrettyPaths, Source, TerminalInfo, msg_id_to_string, string_to_maximum_msg_id,
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
use watcher::Watcher;

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

#[derive(Default)]
struct TransformRuntimeHelpers {
    keep_name: KeepNameHelper,
    pow: String,
}

fn runtime_helper_refs(ast: &crate::internal::js_ast::Ast, alias: &str) -> HashSet<Ref> {
    ast.named_imports
        .iter()
        .filter_map(|(reference, import)| (import.alias == alias).then_some(*reference))
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
                        == alias
                }),
        )
        .collect()
}

fn unique_runtime_helper_name(base: &str, used_names: &mut HashSet<String>) -> String {
    let mut name = base.to_string();
    let mut suffix = 2;
    while used_names.contains(&name) {
        name = format!("{base}{suffix}");
        suffix += 1;
    }
    used_names.insert(name.clone());
    name
}

fn transform_runtime_renamer(
    ast: &crate::internal::js_ast::Ast,
    symbols: SymbolMap,
    keep_names: bool,
    minify_identifiers: bool,
) -> (TransformRenamer, TransformRuntimeHelpers) {
    let mut overrides = HashMap::new();
    let keep_name_refs = if keep_names {
        runtime_helper_refs(ast, "__name")
    } else {
        HashSet::new()
    };
    let pow_refs = runtime_helper_refs(ast, "__pow");
    let keep_name_use_count = keep_name_refs
        .iter()
        .map(|reference| {
            ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                .use_count_estimate
        })
        .sum::<u32>();
    let pow_use_count = pow_refs
        .iter()
        .map(|reference| {
            ast.symbols[usize::try_from(reference.inner_index).expect("symbol index")]
                .use_count_estimate
        })
        .sum::<u32>();
    let (base, mut helpers) = transform_base_renamer(
        ast,
        &symbols,
        minify_identifiers,
        (!keep_name_refs.is_empty()).then_some(keep_name_use_count),
        (!pow_refs.is_empty()).then_some(pow_use_count),
    );
    if !minify_identifiers && (!keep_name_refs.is_empty() || !pow_refs.is_empty()) {
        let helper_indices = keep_name_refs
            .iter()
            .chain(&pow_refs)
            .map(|reference| reference.inner_index)
            .collect::<HashSet<_>>();
        let mut used_names = ast
            .symbols
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                !helper_indices.contains(&u32::try_from(*index).expect("symbol index fits u32"))
            })
            .map(|(_, symbol)| symbol.original_name.clone())
            .collect::<HashSet<_>>();
        if !pow_refs.is_empty() {
            helpers.pow = unique_runtime_helper_name("__pow", &mut used_names);
        }
        if !keep_name_refs.is_empty() {
            helpers.keep_name.name = unique_runtime_helper_name("__name", &mut used_names);
            helpers.keep_name.def_prop = unique_runtime_helper_name("__defProp", &mut used_names);
            helpers.keep_name.target = "target".into();
            helpers.keep_name.value = "value".into();
        }
    }
    overrides.extend(
        keep_name_refs
            .into_iter()
            .map(|reference| (reference, helpers.keep_name.name.clone())),
    );
    overrides.extend(
        pow_refs
            .into_iter()
            .map(|reference| (reference, helpers.pow.clone())),
    );
    (
        TransformRenamer {
            base,
            symbols,
            overrides,
        },
        helpers,
    )
}

fn transform_base_renamer(
    ast: &crate::internal::js_ast::Ast,
    symbols: &SymbolMap,
    minify_identifiers: bool,
    keep_name_use_count: Option<u32>,
    pow_use_count: Option<u32>,
) -> (Box<dyn Renamer>, TransformRuntimeHelpers) {
    if minify_identifiers {
        let scopes = ast.module_scope.iter().cloned().collect::<Vec<_>>();
        let mut reserved_names = crate::internal::renamer::compute_reserved_names(&scopes, symbols);
        if let Some(module_scope) = &ast.module_scope {
            let module_scope = module_scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for member in module_scope.members.values() {
                let reference = symbols.follow_symbols_const(member.reference);
                let symbol = symbols.get(reference);
                if symbol.kind != crate::internal::ast::SymbolKind::Import {
                    reserved_names.insert(symbol.original_name.clone(), 1);
                }
            }
        }
        let mut renamer = crate::internal::renamer::MinifyRenamer::new(
            symbols.clone(),
            ast.nested_scope_slot_counts,
            reserved_names,
        );
        let mut top_level_symbols = Vec::new();
        for part in &ast.parts {
            renamer.accumulate_symbol_use_counts(&mut top_level_symbols, &part.symbol_uses, &[0]);
            for declared in &part.declared_symbols {
                renamer.accumulate_symbol_declaration_count(
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
            renamer.accumulate_synthetic_default_nested_slot(0, 2);
            renamer.accumulate_synthetic_default_nested_slot(1, 2);
            let def_prop = renamer.allocate_synthetic_default_top_level_slot(2);
            let name = renamer.allocate_synthetic_default_top_level_slot(use_count.wrapping_add(2));
            (def_prop, name)
        });
        let pow_slot = pow_use_count
            .map(|use_count| renamer.allocate_synthetic_default_top_level_slot(use_count));
        let minifier =
            DEFAULT_NAME_MINIFIER_JS.shuffle_by_char_freq(ast.char_freq.unwrap_or_default());
        renamer.assign_names_by_frequency(&minifier);
        let keep_name = keep_name_slots
            .map(|(def_prop, name)| KeepNameHelper {
                def_prop: renamer.name_for_synthetic_default_slot(def_prop),
                name: renamer.name_for_synthetic_default_slot(name),
                target: renamer.name_for_synthetic_default_slot(0),
                value: renamer.name_for_synthetic_default_slot(1),
            })
            .unwrap_or_default();
        let pow = pow_slot
            .map(|slot| renamer.name_for_synthetic_default_slot(slot))
            .unwrap_or_default();
        (
            Box::new(renamer),
            TransformRuntimeHelpers { keep_name, pow },
        )
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
        (Box::new(renamer), TransformRuntimeHelpers::default())
    }
}

fn prepend_transform_runtime_helpers(
    code: &mut Vec<u8>,
    helpers: &TransformRuntimeHelpers,
    minify_whitespace: bool,
) {
    let mut prefix = String::new();
    let KeepNameHelper {
        def_prop,
        name,
        target,
        value,
    } = &helpers.keep_name;
    if !name.is_empty() {
        if minify_whitespace {
            write!(prefix, "var {def_prop}=Object.defineProperty;")
                .expect("writing to a string cannot fail");
        } else {
            writeln!(prefix, "var {def_prop} = Object.defineProperty;")
                .expect("writing to a string cannot fail");
        }
    }
    if !helpers.pow.is_empty() {
        if minify_whitespace {
            write!(prefix, "var {}=Math.pow;", helpers.pow)
                .expect("writing to a string cannot fail");
        } else {
            writeln!(prefix, "var {} = Math.pow;", helpers.pow)
                .expect("writing to a string cannot fail");
        }
    }
    if !name.is_empty() {
        let value_property = if value == "value" {
            "value".into()
        } else {
            format!("value: {value}")
        };
        if minify_whitespace {
            let value_property = value_property.replace(' ', "");
            write!(
                prefix,
                "var {name}=({target},{value})=>{def_prop}({target},\"name\",{{{value_property},configurable:true}});"
            )
            .expect("writing to a string cannot fail");
        } else {
            writeln!(
                prefix,
                "var {name} = ({target}, {value}) => {def_prop}({target}, \"name\", {{ {value_property}, configurable: true }});"
            )
            .expect("writing to a string cannot fail");
        }
    }
    if prefix.is_empty() {
        return;
    }
    let insertion = if code.starts_with(b"#!") {
        code.iter()
            .position(|byte| *byte == b'\n')
            .map_or(code.len(), |index| index + 1)
    } else {
        0
    };
    code.splice(insertion..insertion, prefix.bytes());
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Target {
    #[default]
    Default,
    EsNext,
    Es5,
    Es2015,
    Es2016,
    Es2017,
    Es2018,
    Es2019,
    Es2020,
    Es2021,
    Es2022,
    Es2023,
    Es2024,
    Es2025,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum EngineName {
    #[default]
    Chrome,
    Deno,
    Edge,
    Firefox,
    Hermes,
    Ie,
    Ios,
    Node,
    Opera,
    Rhino,
    Safari,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Engine {
    pub name: EngineName,
    pub version: String,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct TransformOptions {
    pub sourcefile: String,
    pub loader: Loader,
    pub abs_paths: AbsPaths,
    pub format: BuildFormat,
    pub global_name: String,
    pub target: Target,
    pub engines: Vec<Engine>,
    pub supported: HashMap<String, bool>,
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
    pub tree_shaking: BuildTreeShaking,
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
            abs_paths: AbsPaths::default(),
            format: BuildFormat::default(),
            global_name: String::new(),
            target: Target::default(),
            engines: Vec::new(),
            supported: HashMap::new(),
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
            tree_shaking: BuildTreeShaking::default(),
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

const fn public_resolve_kind(kind: ImportKind) -> ResolveKind {
    match kind {
        ImportKind::EntryPoint => ResolveKind::EntryPoint,
        ImportKind::Stmt => ResolveKind::ImportStatement,
        ImportKind::Require => ResolveKind::RequireCall,
        ImportKind::Dynamic => ResolveKind::DynamicImport,
        ImportKind::RequireResolve => ResolveKind::RequireResolve,
        ImportKind::At => ResolveKind::CssImportRule,
        ImportKind::ComposesFrom => ResolveKind::CssComposesFrom,
        ImportKind::Url => ResolveKind::CssUrlToken,
    }
}

const fn internal_resolve_kind(kind: ResolveKind) -> Option<ImportKind> {
    match kind {
        ResolveKind::None => None,
        ResolveKind::EntryPoint => Some(ImportKind::EntryPoint),
        ResolveKind::ImportStatement => Some(ImportKind::Stmt),
        ResolveKind::RequireCall => Some(ImportKind::Require),
        ResolveKind::DynamicImport => Some(ImportKind::Dynamic),
        ResolveKind::RequireResolve => Some(ImportKind::RequireResolve),
        ResolveKind::CssImportRule => Some(ImportKind::At),
        ResolveKind::CssComposesFrom => Some(ImportKind::ComposesFrom),
        ResolveKind::CssUrlToken => Some(ImportKind::Url),
    }
}

fn internal_plugin_message(message: Message, kind: MsgKind) -> Msg {
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
}

fn internal_plugin_messages(errors: Vec<Message>, warnings: Vec<Message>) -> Vec<Msg> {
    errors
        .into_iter()
        .map(|message| internal_plugin_message(message, MsgKind::Error))
        .chain(
            warnings
                .into_iter()
                .map(|message| internal_plugin_message(message, MsgKind::Warning)),
        )
        .collect()
}

fn plugin_setup_error(plugin_name: &str, text: impl Into<String>) -> Message {
    Message {
        plugin_name: plugin_name.to_string(),
        text: text.into(),
        kind: MessageKind::Error,
        ..Message::default()
    }
}

fn validate_plugin_path(
    file_system: &dyn Fs,
    plugin_name: &str,
    path: &str,
    kind: &str,
    messages: &mut Vec<Msg>,
) -> String {
    if path.is_empty() {
        return String::new();
    }
    if let Some(path) = file_system.abs(path) {
        return path;
    }
    messages.push(Msg {
        plugin_name: plugin_name.to_string(),
        ..Msg::new(
            MsgKind::Error,
            format!("Invalid {kind} path for plugin {plugin_name:?}: {path}"),
        )
    });
    String::new()
}

fn validate_plugin_paths(
    file_system: &dyn Fs,
    plugin_name: &str,
    paths: Vec<String>,
    kind: &str,
    messages: &mut Vec<Msg>,
) -> Vec<String> {
    paths
        .into_iter()
        .filter_map(|path| {
            let absolute = validate_plugin_path(file_system, plugin_name, &path, kind, messages);
            (!absolute.is_empty()).then_some(absolute)
        })
        .collect()
}

#[derive(Clone)]
struct PreparedPlugins {
    plugins: Vec<config::Plugin>,
    on_end: Vec<PreparedOnEnd>,
    on_dispose: Vec<OnDisposeCallback>,
    resolve_state: Arc<Mutex<PluginResolvePhase>>,
}

impl Default for PreparedPlugins {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            on_end: Vec::new(),
            on_dispose: Vec::new(),
            resolve_state: Arc::new(Mutex::new(PluginResolvePhase::Setup)),
        }
    }
}

#[derive(Clone)]
struct PreparedOnEnd {
    plugin_name: String,
    callback: OnEndCallback,
}

enum PluginResolvePhase {
    Setup,
    Active(Arc<PluginResolveRuntime>),
    Inactive,
}

struct PluginResolveRuntime {
    file_system: RwLock<Arc<dyn Fs>>,
    cache: Arc<CacheSet>,
    options: config::Options,
}

struct PluginResolveFsGuard {
    runtime: Arc<PluginResolveRuntime>,
    previous: Arc<dyn Fs>,
}

impl Drop for PluginResolveFsGuard {
    fn drop(&mut self) {
        *self
            .runtime
            .file_system
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.previous.clone();
    }
}

fn enter_plugin_resolve_fs(
    prepared_plugins: &PreparedPlugins,
    file_system: Arc<dyn Fs>,
) -> Option<PluginResolveFsGuard> {
    let runtime = {
        let phase = prepared_plugins
            .resolve_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*phase {
            PluginResolvePhase::Active(runtime) => runtime.clone(),
            PluginResolvePhase::Setup | PluginResolvePhase::Inactive => return None,
        }
    };
    let previous = std::mem::replace(
        &mut *runtime
            .file_system
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        file_system,
    );
    Some(PluginResolveFsGuard { runtime, previous })
}

fn plugin_resolve_callback(
    state: &Arc<Mutex<PluginResolvePhase>>,
    default_plugin_name: String,
) -> ResolveCallback {
    let state: Weak<Mutex<PluginResolvePhase>> = Arc::downgrade(state);
    Arc::new(move |path, options| {
        let Some(state) = state.upgrade() else {
            return plugin_resolve_error("Cannot call \"resolve\" on an inactive build");
        };
        let runtime = {
            let phase = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*phase {
                PluginResolvePhase::Setup => {
                    return plugin_resolve_error(
                        "Cannot call \"resolve\" before plugin setup has completed",
                    );
                }
                PluginResolvePhase::Active(runtime) => runtime.clone(),
                PluginResolvePhase::Inactive => {
                    return plugin_resolve_error("Cannot call \"resolve\" on an inactive build");
                }
            }
        };
        run_plugin_resolve(&runtime, &default_plugin_name, path, options)
    })
}

fn prepare_plugins(
    options: &mut BuildOptions,
    file_system: &Arc<dyn Fs>,
) -> (PreparedPlugins, Vec<Message>) {
    let declared_plugins = options.plugins.clone();
    let mut prepared_plugins = PreparedPlugins {
        plugins: Vec::with_capacity(declared_plugins.len()),
        ..PreparedPlugins::default()
    };
    let mut errors = Vec::new();
    for (index, plugin) in declared_plugins.into_iter().enumerate() {
        if plugin.name.is_empty() {
            errors.push(build_option_error(format!(
                "Plugin at index {index} is missing a name"
            )));
            continue;
        }
        let mut prepared = config::Plugin {
            name: plugin.name.clone(),
            ..config::Plugin::default()
        };
        let resolve = plugin_resolve_callback(&prepared_plugins.resolve_state, plugin.name.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut build = PluginBuild {
                initial_options: options,
                resolve,
                plugin_name: &plugin.name,
                file_system: file_system.clone(),
                plugin: &mut prepared,
                on_end: &mut prepared_plugins.on_end,
                on_dispose: &mut prepared_plugins.on_dispose,
                errors: &mut errors,
            };
            (plugin.setup)(&mut build)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(plugin_setup_error(&plugin.name, error.message)),
            Err(_) => errors.push(plugin_setup_error(
                &plugin.name,
                "Plugin setup callback panicked",
            )),
        }
        prepared_plugins.plugins.push(prepared);
    }
    (prepared_plugins, errors)
}

fn run_on_end_callbacks(result: &mut BuildResult, prepared_plugins: &PreparedPlugins) {
    for callback in &prepared_plugins.on_end {
        let response =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (callback.callback)(result)));
        let mut response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => OnEndResult {
                errors: vec![plugin_setup_error(&callback.plugin_name, error.message)],
                ..OnEndResult::default()
            },
            Err(_) => OnEndResult {
                errors: vec![plugin_setup_error(
                    &callback.plugin_name,
                    "Plugin onEnd callback panicked",
                )],
                ..OnEndResult::default()
            },
        };
        for message in &mut response.errors {
            message.kind = MessageKind::Error;
            if message.plugin_name.is_empty() {
                message.plugin_name.clone_from(&callback.plugin_name);
            }
        }
        for message in &mut response.warnings {
            message.kind = MessageKind::Warning;
            if message.plugin_name.is_empty() {
                message.plugin_name.clone_from(&callback.plugin_name);
            }
        }
        let did_fail = !response.errors.is_empty();
        result.errors.append(&mut response.errors);
        result.warnings.append(&mut response.warnings);
        if did_fail {
            break;
        }
    }
}

fn run_on_dispose_callbacks(prepared_plugins: &PreparedPlugins) {
    for callback in &prepared_plugins.on_dispose {
        let callback = callback.clone();
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()));
        });
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
pub struct AbsPaths(u8);

impl AbsPaths {
    pub const CODE: Self = Self(1 << 0);
    pub const LOG: Self = Self(1 << 1);
    pub const METAFILE: Self = Self(1 << 2);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
}

impl std::ops::BitOr for AbsPaths {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl std::ops::BitOrAssign for AbsPaths {
    fn bitor_assign(&mut self, right: Self) {
        self.0 |= right.0;
    }
}

const fn internal_path_style(abs_paths: AbsPaths, flag: AbsPaths) -> PathStyle {
    if abs_paths.contains(flag) {
        PathStyle::Absolute
    } else {
        PathStyle::Relative
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildJsx {
    #[default]
    Transform,
    Preserve,
    Automatic,
}

pub type PluginData = Arc<dyn Any + Send + Sync>;
pub type PluginSetupCallback =
    Arc<dyn for<'a> Fn(&mut PluginBuild<'a>) -> Result<(), PluginError> + Send + Sync>;
pub type OnEndCallback =
    Arc<dyn Fn(&mut BuildResult) -> Result<OnEndResult, PluginError> + Send + Sync>;
pub type OnDisposeCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginError {
    pub message: String,
}

impl PluginError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PluginError {}

impl From<String> for PluginError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

impl From<&str> for PluginError {
    fn from(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct Plugin {
    pub name: String,
    pub setup: PluginSetupCallback,
}

impl Plugin {
    #[must_use]
    pub fn new<F>(name: impl Into<String>, setup: F) -> Self
    where
        F: for<'a> Fn(&mut PluginBuild<'a>) -> Result<(), PluginError> + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            setup: Arc::new(setup),
        }
    }
}

impl fmt::Debug for Plugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Plugin")
            .field("name", &self.name)
            .field("setup", &"<callback>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResolveKind {
    #[default]
    None,
    EntryPoint,
    ImportStatement,
    RequireCall,
    DynamicImport,
    RequireResolve,
    CssImportRule,
    CssComposesFrom,
    CssUrlToken,
}

pub type ResolveCallback = Arc<dyn Fn(&str, ResolveOptions) -> ResolveResult + Send + Sync>;

#[derive(Clone, Default)]
pub struct ResolveOptions {
    pub plugin_name: String,
    pub importer: String,
    pub namespace: String,
    pub resolve_dir: String,
    pub kind: ResolveKind,
    pub plugin_data: Option<PluginData>,
    pub with: HashMap<String, String>,
}

#[derive(Clone, Default)]
pub struct ResolveResult {
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
    pub path: String,
    pub external: bool,
    pub side_effects: bool,
    pub namespace: String,
    pub suffix: String,
    pub plugin_data: Option<PluginData>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SideEffects {
    #[default]
    True,
    False,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnResolveOptions {
    pub filter: String,
    pub namespace: String,
}

#[derive(Clone, Default)]
pub struct OnResolveArgs {
    pub path: String,
    pub importer: String,
    pub namespace: String,
    pub resolve_dir: String,
    pub kind: ResolveKind,
    pub plugin_data: Option<PluginData>,
    pub with: HashMap<String, String>,
}

#[derive(Clone, Default)]
pub struct OnResolveResult {
    pub plugin_name: String,
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
    pub path: String,
    pub external: bool,
    pub side_effects: SideEffects,
    pub namespace: String,
    pub suffix: String,
    pub plugin_data: Option<PluginData>,
    pub watch_files: Vec<String>,
    pub watch_dirs: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnLoadOptions {
    pub filter: String,
    pub namespace: String,
}

#[derive(Clone, Default)]
pub struct OnLoadArgs {
    pub path: String,
    pub namespace: String,
    pub suffix: String,
    pub plugin_data: Option<PluginData>,
    pub with: HashMap<String, String>,
}

#[derive(Clone, Default)]
pub struct OnLoadResult {
    pub plugin_name: String,
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
    pub contents: Option<String>,
    pub resolve_dir: String,
    pub loader: Loader,
    pub plugin_data: Option<PluginData>,
    pub watch_files: Vec<String>,
    pub watch_dirs: Vec<String>,
}

#[derive(Clone, Default)]
pub struct OnStartResult {
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
}

#[derive(Clone, Default)]
pub struct OnEndResult {
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
}

pub struct PluginBuild<'a> {
    pub initial_options: &'a mut BuildOptions,
    pub resolve: ResolveCallback,
    plugin_name: &'a str,
    file_system: Arc<dyn Fs>,
    plugin: &'a mut config::Plugin,
    on_end: &'a mut Vec<PreparedOnEnd>,
    on_dispose: &'a mut Vec<OnDisposeCallback>,
    errors: &'a mut Vec<Message>,
}

impl PluginBuild<'_> {
    pub fn on_start<F>(&mut self, callback: F)
    where
        F: Fn() -> Result<OnStartResult, PluginError> + Send + Sync + 'static,
    {
        let callback = Arc::new(callback);
        self.plugin.on_start.push(config::OnStart {
            callback: Some(Arc::new(move || match callback() {
                Ok(response) => config::OnStartResult {
                    messages: internal_plugin_messages(response.errors, response.warnings),
                    ..config::OnStartResult::default()
                },
                Err(error) => config::OnStartResult {
                    thrown_error: Some(error.message),
                    ..config::OnStartResult::default()
                },
            })),
            name: self.plugin_name.to_string(),
        });
    }

    pub fn on_end<F>(&mut self, callback: F)
    where
        F: Fn(&mut BuildResult) -> Result<OnEndResult, PluginError> + Send + Sync + 'static,
    {
        self.on_end.push(PreparedOnEnd {
            plugin_name: self.plugin_name.to_string(),
            callback: Arc::new(callback),
        });
    }

    pub fn on_dispose<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_dispose.push(Arc::new(callback));
    }

    #[allow(clippy::too_many_lines)]
    pub fn on_resolve<F>(&mut self, options: OnResolveOptions, callback: F)
    where
        F: Fn(OnResolveArgs) -> Result<OnResolveResult, PluginError> + Send + Sync + 'static,
    {
        let filter =
            match config::compile_filter_for_plugin(self.plugin_name, "OnResolve", &options.filter)
            {
                Ok(filter) => filter,
                Err(message) => {
                    self.errors
                        .push(plugin_setup_error(self.plugin_name, message));
                    return;
                }
            };
        let callback = Arc::new(callback);
        let file_system = self.file_system.clone();
        let plugin_name = self.plugin_name.to_string();
        self.plugin.on_resolve.push(config::OnResolve {
            filter: Some(filter),
            callback: Some(Arc::new(move |args| {
                let response = match callback(OnResolveArgs {
                    path: args.path,
                    importer: args.importer.text,
                    namespace: args.importer.namespace,
                    resolve_dir: args.resolve_dir,
                    kind: public_resolve_kind(args.kind),
                    plugin_data: args.plugin_data,
                    with: args.with.decode_into_map(),
                }) {
                    Ok(response) => response,
                    Err(error) => {
                        return config::OnResolveResult {
                            thrown_error: Some(error.message),
                            ..config::OnResolveResult::default()
                        };
                    }
                };
                let mut messages =
                    internal_plugin_messages(response.errors, response.warnings);
                let returned_watch_files = !response.watch_files.is_empty();
                let returned_watch_dirs = !response.watch_dirs.is_empty();
                let abs_watch_files = validate_plugin_paths(
                    file_system.as_ref(),
                    &plugin_name,
                    response.watch_files,
                    "watch file",
                    &mut messages,
                );
                let abs_watch_dirs = validate_plugin_paths(
                    file_system.as_ref(),
                    &plugin_name,
                    response.watch_dirs,
                    "watch directory",
                    &mut messages,
                );
                if !response.suffix.is_empty()
                    && !response.suffix.starts_with(['?', '#'])
                {
                    return config::OnResolveResult {
                        plugin_name: response.plugin_name,
                        messages,
                        thrown_error: Some(format!(
                            "Invalid path suffix {:?} returned from plugin (must start with \"?\" or \"#\")",
                            response.suffix
                        )),
                        abs_watch_files,
                        abs_watch_dirs,
                        ..config::OnResolveResult::default()
                    };
                }
                if response.path.is_empty() && !response.external {
                    let unused = if !response.namespace.is_empty() {
                        "namespace"
                    } else if !response.suffix.is_empty() {
                        "suffix"
                    } else if response.plugin_data.is_some() {
                        "pluginData"
                    } else if returned_watch_files {
                        "watchFiles"
                    } else if returned_watch_dirs {
                        "watchDirs"
                    } else {
                        ""
                    };
                    if !unused.is_empty() {
                        messages.push(Msg::new(
                            MsgKind::Warning,
                            format!(
                                "Returning {unused:?} doesn't do anything when \"path\" is empty"
                            ),
                        ));
                    }
                }
                config::OnResolveResult {
                    plugin_name: response.plugin_name,
                    messages,
                    abs_watch_files,
                    abs_watch_dirs,
                    plugin_data: response.plugin_data,
                    path: Path {
                        text: response.path,
                        namespace: response.namespace,
                        ignored_suffix: response.suffix,
                        ..Path::default()
                    },
                    external: response.external,
                    is_side_effect_free: response.side_effects == SideEffects::False,
                    ..config::OnResolveResult::default()
                }
            })),
            name: self.plugin_name.to_string(),
            namespace: options.namespace,
        });
    }

    pub fn on_load<F>(&mut self, options: OnLoadOptions, callback: F)
    where
        F: Fn(OnLoadArgs) -> Result<OnLoadResult, PluginError> + Send + Sync + 'static,
    {
        let filter =
            match config::compile_filter_for_plugin(self.plugin_name, "OnLoad", &options.filter) {
                Ok(filter) => filter,
                Err(message) => {
                    self.errors
                        .push(plugin_setup_error(self.plugin_name, message));
                    return;
                }
            };
        let callback = Arc::new(callback);
        let file_system = self.file_system.clone();
        let plugin_name = self.plugin_name.to_string();
        self.plugin.on_load.push(config::OnLoad {
            filter: Some(filter),
            callback: Some(Arc::new(move |args| {
                let response = match callback(OnLoadArgs {
                    path: args.path.text,
                    namespace: args.path.namespace,
                    suffix: args.path.ignored_suffix,
                    plugin_data: args.plugin_data,
                    with: args.path.import_attributes.decode_into_map(),
                }) {
                    Ok(response) => response,
                    Err(error) => {
                        return config::OnLoadResult {
                            thrown_error: Some(error.message),
                            ..config::OnLoadResult::default()
                        };
                    }
                };
                let mut messages = internal_plugin_messages(response.errors, response.warnings);
                let abs_watch_files = validate_plugin_paths(
                    file_system.as_ref(),
                    &plugin_name,
                    response.watch_files,
                    "watch file",
                    &mut messages,
                );
                let abs_watch_dirs = validate_plugin_paths(
                    file_system.as_ref(),
                    &plugin_name,
                    response.watch_dirs,
                    "watch directory",
                    &mut messages,
                );
                let abs_resolve_dir = validate_plugin_path(
                    file_system.as_ref(),
                    &plugin_name,
                    &response.resolve_dir,
                    "resolve directory",
                    &mut messages,
                );
                config::OnLoadResult {
                    plugin_name: response.plugin_name,
                    contents: response.contents,
                    abs_resolve_dir,
                    plugin_data: response.plugin_data,
                    messages,
                    abs_watch_files,
                    abs_watch_dirs,
                    loader: build_loader(response.loader),
                    ..config::OnLoadResult::default()
                }
            })),
            name: self.plugin_name.to_string(),
            namespace: options.namespace,
        });
    }
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
    pub abs_paths: AbsPaths,
    pub format: BuildFormat,
    pub platform: BuildPlatform,
    pub target: Target,
    pub engines: Vec<Engine>,
    pub supported: HashMap<String, bool>,
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
    pub plugins: Vec<Plugin>,
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
            abs_paths: AbsPaths::default(),
            format: BuildFormat::default(),
            platform: BuildPlatform::default(),
            target: Target::default(),
            engines: Vec::new(),
            supported: HashMap::new(),
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
            plugins: Vec::new(),
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

#[derive(Clone, Debug, Default)]
pub struct ContextError {
    pub errors: Vec<Message>,
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(error) = self.errors.first() {
            formatter.write_str(&error.text)
        } else {
            formatter.write_str("Context creation failed")
        }
    }
}

impl std::error::Error for ContextError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WatchOptions {
    pub delay: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WatchError {
    pub message: String,
}

impl fmt::Display for WatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WatchError {}

#[derive(Clone)]
pub struct BuildContext {
    inner: Arc<BuildContextInner>,
}

struct BuildContextInner {
    options: BuildOptions,
    prepared_plugins: PreparedPlugins,
    cache: Arc<CacheSet>,
    state: Mutex<BuildContextState>,
}

#[derive(Default)]
struct BuildContextState {
    disposed: bool,
    active: Option<Arc<InFlightBuild>>,
    latest_hashes: HashMap<String, String>,
    watcher: Option<Arc<Watcher>>,
}

#[derive(Default)]
struct InFlightBuild {
    outcome: Mutex<Option<InFlightOutcome>>,
    changed: Condvar,
}

#[derive(Clone)]
enum InFlightOutcome {
    Completed(BuildResult),
    Panicked,
}

impl InFlightBuild {
    fn finish(&self, outcome: InFlightOutcome) {
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
        self.changed.notify_all();
    }

    fn wait(&self) -> InFlightOutcome {
        let mut outcome = self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while outcome.is_none() {
            outcome = self
                .changed
                .wait(outcome)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        outcome.clone().unwrap_or(InFlightOutcome::Panicked)
    }

    fn wait_for_result(&self) -> BuildResult {
        match self.wait() {
            InFlightOutcome::Completed(result) => result,
            InFlightOutcome::Panicked => panic!("concurrent build context rebuild panicked"),
        }
    }
}

impl BuildContext {
    #[must_use]
    pub fn rebuild(&self) -> BuildResult {
        self.rebuild_with(
            || {
                let (old_hashes, watcher) = {
                    let state = self
                        .inner
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    (state.latest_hashes.clone(), state.watcher.clone())
                };
                let watch_data = Mutex::new(WatchData::default());
                let (result, latest_hashes) = build_with_output_state(
                    self.inner.options.clone(),
                    &self.inner.cache,
                    &self.inner.prepared_plugins,
                    Some(&old_hashes),
                    watcher.as_ref().map(|_| &watch_data),
                );
                if let Some(watcher) = watcher {
                    watcher.set_watch_data(
                        watch_data
                            .into_inner()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                }
                self.inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .latest_hashes = latest_hashes;
                result
            },
            || {},
        )
    }

    /// Enables polling watch mode and starts the initial watch build asynchronously.
    ///
    /// # Errors
    ///
    /// Returns an error if this context is disposed or watch mode is already enabled.
    pub fn watch(&self, options: WatchOptions) -> Result<(), WatchError> {
        let (watcher, previous_build) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.disposed {
                return Err(WatchError {
                    message: "Cannot watch a disposed context".into(),
                });
            }
            if state.watcher.is_some() {
                return Err(WatchError {
                    message: "Watch mode has already been enabled".into(),
                });
            }
            let watcher = Watcher::new(options.delay);
            let previous_build = state.active.clone();
            state.watcher = Some(watcher.clone());
            (watcher, previous_build)
        };

        let weak_inner = Arc::downgrade(&self.inner);
        watcher.start(Arc::new(move || {
            if let Some(inner) = weak_inner.upgrade() {
                let _ = BuildContext { inner }.rebuild();
            }
        }));

        let weak_inner = Arc::downgrade(&self.inner);
        std::thread::spawn(move || {
            if let Some(previous_build) = previous_build {
                let _ = previous_build.wait();
            }
            if let Some(inner) = weak_inner.upgrade() {
                let _ = BuildContext { inner }.rebuild();
            }
        });
        Ok(())
    }

    fn rebuild_with(
        &self,
        build: impl FnOnce() -> BuildResult,
        joined_existing_build: impl FnOnce(),
    ) -> BuildResult {
        let (in_flight, should_build) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.disposed {
                return BuildResult::default();
            }
            if let Some(in_flight) = &state.active {
                (in_flight.clone(), false)
            } else {
                let in_flight = Arc::new(InFlightBuild::default());
                state.active = Some(in_flight.clone());
                (in_flight, true)
            }
        };
        if !should_build {
            joined_existing_build();
            return in_flight.wait_for_result();
        }

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
        match outcome {
            Ok(result) => {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state
                    .active
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &in_flight))
                {
                    state.active = None;
                }
                in_flight.finish(InFlightOutcome::Completed(result.clone()));
                result
            }
            Err(payload) => {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state
                    .active
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &in_flight))
                {
                    state.active = None;
                }
                in_flight.finish(InFlightOutcome::Panicked);
                drop(state);
                std::panic::resume_unwind(payload);
            }
        }
    }

    pub fn dispose(&self) {
        self.dispose_with(|| {});
    }

    fn dispose_with(&self, waiting_for_build: impl FnOnce()) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.disposed {
            return;
        }
        state.disposed = true;
        let active = state.active.clone();
        let watcher = state.watcher.clone();
        drop(state);
        if let Some(watcher) = watcher {
            watcher.stop();
        }
        if let Some(active) = active {
            waiting_for_build();
            let _ = active.wait();
        }
        deactivate_plugin_resolve(&self.inner.prepared_plugins);
        run_on_dispose_callbacks(&self.inner.prepared_plugins);
    }
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

struct WatchDataRecorder<'a> {
    file_system: &'a dyn Fs,
    sink: &'a Mutex<WatchData>,
}

impl Drop for WatchDataRecorder<'_> {
    fn drop(&mut self) {
        *self
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.file_system.watch_data();
    }
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
    previous_hashes: Option<&HashMap<String, String>>,
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
    if let Some(previous_hashes) = previous_hashes {
        let current_paths = output_files
            .iter()
            .map(|output| output.path.as_str())
            .collect::<HashSet<_>>();
        for path in previous_hashes.keys() {
            if !current_paths.contains(path.as_str()) {
                let _ = std_fs::remove_file(path);
            }
        }
    }
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
        if previous_hashes
            .and_then(|hashes| hashes.get(&output.path))
            .is_some_and(|hash| hash == &output.hash)
            && std_fs::read(path).is_ok_and(|contents| contents == output.contents)
        {
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

#[derive(Clone, Debug, Default)]
struct ValidatedTargetFeatures {
    unsupported_js_features: crate::internal::compat::JsFeature,
    unsupported_css_features: crate::internal::compat::CssFeature,
    css_prefix_data:
        HashMap<crate::internal::css_ast::Declaration, crate::internal::compat::CssPrefix>,
    unsupported_js_feature_overrides: crate::internal::compat::JsFeature,
    unsupported_js_feature_overrides_mask: crate::internal::compat::JsFeature,
    unsupported_css_feature_overrides: crate::internal::compat::CssFeature,
    unsupported_css_feature_overrides_mask: crate::internal::compat::CssFeature,
    original_target_environment: String,
}

const fn engine_name_to_compat(name: EngineName) -> crate::internal::compat::Engine {
    match name {
        EngineName::Chrome => crate::internal::compat::Engine::Chrome,
        EngineName::Deno => crate::internal::compat::Engine::Deno,
        EngineName::Edge => crate::internal::compat::Engine::Edge,
        EngineName::Firefox => crate::internal::compat::Engine::Firefox,
        EngineName::Hermes => crate::internal::compat::Engine::Hermes,
        EngineName::Ie => crate::internal::compat::Engine::Ie,
        EngineName::Ios => crate::internal::compat::Engine::Ios,
        EngineName::Node => crate::internal::compat::Engine::Node,
        EngineName::Opera => crate::internal::compat::Engine::Opera,
        EngineName::Rhino => crate::internal::compat::Engine::Rhino,
        EngineName::Safari => crate::internal::compat::Engine::Safari,
    }
}

fn parse_engine_version(version: &str) -> Option<crate::internal::compat::Semver> {
    let (numbers, pre_release) = version
        .split_once('-')
        .map_or((version, ""), |(numbers, suffix)| (numbers, suffix));
    if numbers.is_empty()
        || numbers.matches('.').count() > 2
        || (!pre_release.is_empty()
            && pre_release.split('.').any(|part| {
                part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_alphanumeric())
            }))
        || (version.contains('-') && pre_release.is_empty())
    {
        return None;
    }
    let parts = numbers
        .split('.')
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| part.parse::<i32>().ok())
                .flatten()
        })
        .collect::<Option<Vec<_>>>()?;
    (parts.len() <= 3).then_some(crate::internal::compat::Semver {
        parts,
        pre_release: if pre_release.is_empty() {
            String::new()
        } else {
            format!("-{pre_release}")
        },
    })
}

fn validate_supported_features(
    log: &Log,
    supported: &HashMap<String, bool>,
) -> (
    crate::internal::compat::JsFeature,
    crate::internal::compat::JsFeature,
    crate::internal::compat::CssFeature,
    crate::internal::compat::CssFeature,
) {
    let mut js_overrides = crate::internal::compat::JsFeature::NONE;
    let mut js_mask = crate::internal::compat::JsFeature::NONE;
    let mut css_overrides = crate::internal::compat::CssFeature::NONE;
    let mut css_mask = crate::internal::compat::CssFeature::NONE;
    for (name, is_supported) in supported {
        if let Some(feature) = crate::internal::compat::STRING_TO_JS_FEATURE.get(name.as_str()) {
            js_mask |= *feature;
            if !is_supported {
                js_overrides |= *feature;
            }
        } else if let Some(feature) =
            crate::internal::compat::STRING_TO_CSS_FEATURE.get(name.as_str())
        {
            css_mask |= *feature;
            if !is_supported {
                css_overrides |= *feature;
            }
        } else {
            log.add_error(
                None,
                crate::internal::logger::Range::default(),
                format!("{name:?} is not a valid feature name for the \"supported\" setting"),
            );
        }
    }
    (js_overrides, js_mask, css_overrides, css_mask)
}

fn validate_target_features(
    log: &Log,
    target: Target,
    engines: &[Engine],
    supported: &HashMap<String, bool>,
    platform: BuildPlatform,
) -> ValidatedTargetFeatures {
    let mut constraints = HashMap::new();
    let mut targets = Vec::with_capacity(engines.len() + 1);
    let es_version = match target {
        Target::Es5 => Some(5),
        Target::Es2015 => Some(2015),
        Target::Es2016 => Some(2016),
        Target::Es2017 => Some(2017),
        Target::Es2018 => Some(2018),
        Target::Es2019 => Some(2019),
        Target::Es2020 => Some(2020),
        Target::Es2021 => Some(2021),
        Target::Es2022 => Some(2022),
        Target::Es2023 => Some(2023),
        Target::Es2024 => Some(2024),
        Target::Es2025 => Some(2025),
        Target::Default | Target::EsNext => None,
    };
    if let Some(version) = es_version {
        constraints.insert(
            crate::internal::compat::Engine::Es,
            crate::internal::compat::Semver {
                parts: vec![version],
                pre_release: String::new(),
            },
        );
    }
    for engine in engines {
        if let Some(version) = parse_engine_version(&engine.version) {
            constraints.insert(engine_name_to_compat(engine.name), version);
        } else {
            log.add_error_with_notes(
                None,
                crate::internal::logger::Range::default(),
                format!("Invalid version: {:?}", engine.version),
                vec![MsgData {
                    text: "All version numbers passed to esbuild must be in the format \"X\", \
                           \"X.Y\", or \"X.Y.Z\" where X, Y, and Z are non-negative integers."
                        .into(),
                    ..MsgData::default()
                }],
            );
        }
    }
    for (engine, version) in &constraints {
        targets.push(format!("{engine}{version}"));
    }
    if target == Target::EsNext {
        targets.push("esnext".into());
    }
    targets.sort();

    let (js_overrides, js_mask, css_overrides, css_mask) =
        validate_supported_features(log, supported);

    let mut internal_options = config::Options {
        platform: match platform {
            BuildPlatform::Default | BuildPlatform::Browser => config::Platform::Browser,
            BuildPlatform::Node => config::Platform::Node,
            BuildPlatform::Neutral => config::Platform::Neutral,
        },
        unsupported_js_features: crate::internal::compat::unsupported_js_features(&constraints)
            .apply_overrides(js_overrides, js_mask),
        unsupported_css_features: crate::internal::compat::unsupported_css_features(&constraints)
            .apply_overrides(css_overrides, css_mask),
        unsupported_js_feature_overrides: js_overrides,
        unsupported_js_feature_overrides_mask: js_mask,
        unsupported_css_feature_overrides: css_overrides,
        unsupported_css_feature_overrides_mask: css_mask,
        ..config::Options::default()
    };
    bundler::apply_unsupported_feature_constraints(&mut internal_options);
    ValidatedTargetFeatures {
        unsupported_js_features: internal_options.unsupported_js_features,
        unsupported_css_features: internal_options.unsupported_css_features,
        css_prefix_data: crate::internal::compat::css_prefix_data(&constraints),
        unsupported_js_feature_overrides: internal_options.unsupported_js_feature_overrides,
        unsupported_js_feature_overrides_mask: internal_options
            .unsupported_js_feature_overrides_mask,
        unsupported_css_feature_overrides: internal_options.unsupported_css_feature_overrides,
        unsupported_css_feature_overrides_mask: internal_options
            .unsupported_css_feature_overrides_mask,
        original_target_environment:
            crate::internal::helpers::string_array_to_quoted_comma_separated_string(&targets),
    }
}

fn build_option_error(text: impl Into<String>) -> Message {
    Message {
        text: text.into(),
        kind: MessageKind::Error,
        ..Message::default()
    }
}

#[allow(clippy::too_many_lines)]
fn validate_context_options(options: &BuildOptions, file_system: &dyn Fs) -> Vec<Message> {
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let _ = validate_target_features(
        &log,
        options.target,
        &options.engines,
        &options.supported,
        options.platform,
    );

    let mut errors = Vec::new();
    let entry_point_count = options.entry_points.len()
        + options.entry_points_advanced.len()
        + usize::from(options.stdin.is_some());
    if options.outdir.is_empty() && entry_point_count > 1 {
        errors.push(build_option_error(
            "Must use \"outdir\" when there are multiple input files",
        ));
    } else if options.outdir.is_empty() && options.splitting {
        errors.push(build_option_error(
            "Must use \"outdir\" when code splitting is enabled",
        ));
    } else if !options.outfile.is_empty() && !options.outdir.is_empty() {
        errors.push(build_option_error(
            "Cannot use both \"outfile\" and \"outdir\"",
        ));
    }

    if options.outdir.is_empty() && options.outfile.is_empty() {
        if !matches!(
            options.sourcemap,
            BuildSourceMap::None | BuildSourceMap::Inline
        ) {
            errors.push(build_option_error(
                "Cannot use an external source map without an output path",
            ));
        }
        if matches!(
            options.legal_comments,
            BuildLegalComments::Linked | BuildLegalComments::External
        ) {
            errors.push(build_option_error(
                "Cannot use linked or external legal comments without an output path",
            ));
        }
        if options
            .loader
            .values()
            .any(|loader| *loader == Loader::File)
        {
            errors.push(build_option_error(
                "Cannot use the \"file\" loader without an output path",
            ));
        }
        if options
            .loader
            .values()
            .any(|loader| *loader == Loader::Copy)
        {
            errors.push(build_option_error(
                "Cannot use the \"copy\" loader without an output path",
            ));
        }
    }

    if !options.bundle {
        if !options.external.is_empty() {
            errors.push(build_option_error(
                "Cannot use \"external\" without \"bundle\"",
            ));
        }
        if !options.alias.is_empty() {
            errors.push(build_option_error(
                "Cannot use \"alias\" without \"bundle\"",
            ));
        }
    }

    if let Err(validation_errors) = validate_externals(file_system, &options.external) {
        errors.extend(validation_errors);
    }
    if let Err(validation_errors) = validate_build_loaders(&options.loader) {
        errors.extend(validation_errors);
    }
    if let Err(validation_errors) = validate_output_extensions(&options.out_extension) {
        errors.extend(validation_errors);
    }
    if let Err(validation_errors) = validate_resolve_extensions(&options.resolve_extensions) {
        errors.extend(validation_errors);
    }

    if !options.global_name.is_empty() {
        let _ = js_parser::parse_global_name(
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
    }
    let _ = validate_defines(
        &log,
        &options.define,
        &options.pure,
        options.platform,
        options.minify_whitespace && options.minify_identifiers && options.minify_syntax,
    );
    let _ = validate_jsx_define(&log, &options.jsx_factory, "jsx factory", false);
    let _ = validate_jsx_define(&log, &options.jsx_fragment, "jsx fragment", true);
    if !options.tsconfig.is_empty() && !options.tsconfig_raw.is_empty() {
        log.add_error(
            None,
            crate::internal::logger::Range::default(),
            "Cannot provide \"tsconfig\" as both a raw string and a path",
        );
    }

    let output_format = match options.format {
        BuildFormat::Default if options.bundle => match options.platform {
            BuildPlatform::Default | BuildPlatform::Browser => config::Format::Iife,
            BuildPlatform::Node => config::Format::CommonJs,
            BuildPlatform::Neutral => config::Format::EsModule,
        },
        BuildFormat::Default => config::Format::Preserve,
        BuildFormat::Iife => config::Format::Iife,
        BuildFormat::CommonJs => config::Format::CommonJs,
        BuildFormat::EsModule => config::Format::EsModule,
    };
    if options.splitting && output_format != config::Format::EsModule {
        errors.push(build_option_error(
            "Splitting currently only works with the \"esm\" format",
        ));
    }

    let (mut logged_errors, _) = public_messages_with_path_style(
        log.done(),
        internal_path_style(options.abs_paths, AbsPaths::LOG),
    );
    logged_errors.extend(errors);
    logged_errors
}

fn plugin_resolve_error(text: impl Into<String>) -> ResolveResult {
    ResolveResult {
        errors: vec![Message {
            text: text.into(),
            kind: MessageKind::Error,
            ..Message::default()
        }],
        ..ResolveResult::default()
    }
}

fn activate_plugin_resolve(
    prepared_plugins: &PreparedPlugins,
    options: &BuildOptions,
    file_system: Arc<dyn Fs>,
    cache: Arc<CacheSet>,
) -> Result<(), Vec<Message>> {
    let external_settings = validate_externals(file_system.as_ref(), &options.external)?;
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
    let tsconfig_path = if options.tsconfig.is_empty() {
        String::new()
    } else if file_system.is_abs(&options.tsconfig) {
        options.tsconfig.clone()
    } else {
        file_system.join(&[file_system.cwd(), &options.tsconfig])
    };
    let mut resolve_options = config::Options {
        platform: match options.platform {
            BuildPlatform::Default | BuildPlatform::Browser => config::Platform::Browser,
            BuildPlatform::Node => config::Platform::Node,
            BuildPlatform::Neutral => config::Platform::Neutral,
        },
        extension_order: options.resolve_extensions.clone(),
        main_fields: options.main_fields.clone(),
        conditions: options.conditions.clone(),
        abs_node_paths,
        external_settings,
        external_packages: options.packages == Packages::External,
        package_aliases: options.alias.clone(),
        preserve_symlinks: options.preserve_symlinks,
        tsconfig_path,
        tsconfig_raw: options.tsconfig_raw.clone(),
        log_path_style: internal_path_style(options.abs_paths, AbsPaths::LOG),
        code_path_style: internal_path_style(options.abs_paths, AbsPaths::CODE),
        metafile_path_style: internal_path_style(options.abs_paths, AbsPaths::METAFILE),
        plugins: prepared_plugins.plugins.clone(),
        ..config::Options::default()
    };
    bundler::apply_option_defaults(&mut resolve_options);
    *prepared_plugins
        .resolve_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        PluginResolvePhase::Active(Arc::new(PluginResolveRuntime {
            file_system: RwLock::new(file_system),
            cache,
            options: resolve_options,
        }));
    Ok(())
}

fn deactivate_plugin_resolve(prepared_plugins: &PreparedPlugins) {
    *prepared_plugins
        .resolve_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = PluginResolvePhase::Inactive;
}

fn plugin_resolve_failure(
    runtime: &PluginResolveRuntime,
    file_system: &dyn Fs,
    default_plugin_name: &str,
    plugin_name: &str,
    path: &str,
    kind: ImportKind,
    abs_resolve_dir: &str,
) -> Message {
    let plugin_name = if plugin_name.is_empty() {
        default_plugin_name
    } else {
        plugin_name
    };
    let mut hint = String::new();
    if resolver::is_package_path(path) && !file_system.is_abs(path) {
        hint = format!(
            "You can mark the path {path:?} as external to exclude it from the bundle, \
             which will remove this error and leave the unresolved path in the bundle."
        );
        if kind == ImportKind::Require {
            hint.push_str(
                " You can also surround this \"require\" call with a try/catch block to handle \
                 this failure at run-time instead of bundle-time.",
            );
        } else if kind == ImportKind::Dynamic {
            hint.push_str(
                " You can also add \".catch()\" here to handle this failure at run-time instead \
                 of bundle-time.",
            );
        }
    }
    if runtime.options.platform != config::Platform::Node {
        let package = path.strip_prefix("node:").unwrap_or(path);
        if resolver::is_node_builtin(package) {
            hint = format!(
                "The package {path:?} wasn't found on the file system but is built into node. \
                 Are you trying to bundle for node? You can use \
                 \"platform: BuildPlatform::Node\" to do that, which will remove this error."
            );
        }
    }
    if abs_resolve_dir.is_empty() && !plugin_name.is_empty() {
        hint = format!(
            "The plugin {plugin_name:?} didn't set a resolve directory, so esbuild did not search \
             for {path:?} on the file system."
        );
    }
    Message {
        text: format!("Could not resolve {path:?}"),
        notes: (!hint.is_empty())
            .then(|| Note {
                text: hint,
                ..Note::default()
            })
            .into_iter()
            .collect(),
        kind: MessageKind::Error,
        ..Message::default()
    }
}

fn run_plugin_resolve(
    runtime: &PluginResolveRuntime,
    default_plugin_name: &str,
    path: &str,
    options: ResolveOptions,
) -> ResolveResult {
    let Some(kind) = internal_resolve_kind(options.kind) else {
        return plugin_resolve_error("Must specify \"kind\" when calling \"resolve\"");
    };
    let file_system = runtime
        .file_system
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let abs_resolve_dir = if options.resolve_dir.is_empty() {
        String::new()
    } else {
        let Some(path) = file_system.abs(&options.resolve_dir) else {
            return plugin_resolve_error(format!(
                "Invalid resolve directory: {}",
                options.resolve_dir
            ));
        };
        path
    };
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let raw_tsconfig = if runtime.options.tsconfig_raw.is_empty() {
        None
    } else {
        parse_tsconfig_raw(
            &log,
            file_system.as_ref(),
            file_system.cwd(),
            &runtime.options.tsconfig_raw,
        )
    };
    let importer = Path {
        text: options.importer,
        namespace: options.namespace,
        ..Path::default()
    };
    let attributes = crate::internal::logger::ImportAttributes::encode(&options.with);
    let (resolved, _) = bundler::resolve_for_plugin_api(
        &log,
        file_system.as_ref(),
        runtime.cache.as_ref(),
        &runtime.options,
        &importer,
        path,
        &attributes,
        kind,
        &abs_resolve_dir,
        options.plugin_data,
        raw_tsconfig.as_ref(),
    );
    let (mut errors, warnings) =
        public_messages_with_path_style(log.done(), runtime.options.log_path_style);
    let Some(resolved) = resolved else {
        if errors.is_empty() {
            errors.push(plugin_resolve_failure(
                runtime,
                file_system.as_ref(),
                default_plugin_name,
                &options.plugin_name,
                path,
                kind,
                &abs_resolve_dir,
            ));
        }
        return ResolveResult {
            errors,
            warnings,
            ..ResolveResult::default()
        };
    };
    ResolveResult {
        errors,
        warnings,
        path: resolved.path_pair.primary.text,
        external: resolved.path_pair.is_external,
        side_effects: resolved.primary_side_effects_data.is_none(),
        namespace: resolved.path_pair.primary.namespace,
        suffix: resolved.path_pair.primary.ignored_suffix,
        plugin_data: resolved.plugin_data,
    }
}

/// Creates a reusable build context without resolving or parsing entry points.
///
/// # Errors
///
/// Returns build-option validation errors. File resolution and syntax errors are
/// returned by [`BuildContext::rebuild`] so a later rebuild can recover from them.
pub fn context(mut options: BuildOptions) -> Result<BuildContext, ContextError> {
    let abs_working_dir = options.abs_working_dir.clone();
    let file_system: Arc<dyn Fs> = real_fs(RealFsOptions {
        abs_working_dir: abs_working_dir.clone(),
        do_not_cache: true,
        ..RealFsOptions::default()
    })
    .map_err(|error| ContextError {
        errors: vec![build_option_error(error.message)],
    })?
    .into();
    let (prepared_plugins, mut errors) = prepare_plugins(&mut options, &file_system);
    if options.abs_working_dir != abs_working_dir {
        errors.push(build_option_error(
            "Mutating \"abs_working_dir\" during plugin setup is not allowed",
        ));
        options.abs_working_dir = abs_working_dir;
    }
    errors.extend(validate_context_options(&options, file_system.as_ref()));
    if !errors.is_empty() {
        return Err(ContextError { errors });
    }
    options.abs_working_dir = file_system.cwd().to_string();
    let cache = Arc::new(CacheSet::default());
    if let Err(errors) =
        activate_plugin_resolve(&prepared_plugins, &options, file_system, cache.clone())
    {
        return Err(ContextError { errors });
    }
    Ok(BuildContext {
        inner: Arc::new(BuildContextInner {
            options,
            prepared_plugins,
            cache,
            state: Mutex::new(BuildContextState::default()),
        }),
    })
}

#[must_use]
pub fn build(options: BuildOptions) -> BuildResult {
    build_with_cache(options, &Arc::new(CacheSet::default()))
}

fn build_with_cache(options: BuildOptions, cache: &Arc<CacheSet>) -> BuildResult {
    let mut options = options;
    let abs_working_dir = options.abs_working_dir.clone();
    let file_system: Arc<dyn Fs> = match real_fs(RealFsOptions {
        abs_working_dir: abs_working_dir.clone(),
        do_not_cache: true,
        ..RealFsOptions::default()
    }) {
        Ok(file_system) => file_system.into(),
        Err(error) => {
            return BuildResult {
                errors: vec![build_option_error(error.message)],
                ..BuildResult::default()
            };
        }
    };
    let (prepared_plugins, mut errors) = prepare_plugins(&mut options, &file_system);
    if options.abs_working_dir != abs_working_dir {
        errors.push(build_option_error(
            "Mutating \"abs_working_dir\" during plugin setup is not allowed",
        ));
        options.abs_working_dir = abs_working_dir;
    }
    errors.extend(validate_context_options(&options, file_system.as_ref()));
    if !errors.is_empty() {
        return BuildResult {
            errors,
            ..BuildResult::default()
        };
    }
    options.abs_working_dir = file_system.cwd().to_string();
    if let Err(errors) =
        activate_plugin_resolve(&prepared_plugins, &options, file_system, cache.clone())
    {
        return BuildResult {
            errors,
            ..BuildResult::default()
        };
    }
    let (result, _) =
        build_with_output_state(options, cache.as_ref(), &prepared_plugins, None, None);
    deactivate_plugin_resolve(&prepared_plugins);
    run_on_dispose_callbacks(&prepared_plugins);
    result
}

fn build_with_output_state(
    options: BuildOptions,
    cache: &CacheSet,
    prepared_plugins: &PreparedPlugins,
    previous_hashes: Option<&HashMap<String, String>>,
    watch_data_sink: Option<&Mutex<WatchData>>,
) -> (BuildResult, HashMap<String, String>) {
    let mut result = build_with_output_state_core(
        options,
        cache,
        prepared_plugins,
        previous_hashes,
        watch_data_sink,
    );
    let latest_hashes = result
        .output_files
        .iter()
        .map(|output| (output.path.clone(), output.hash.clone()))
        .collect();
    run_on_end_callbacks(&mut result, prepared_plugins);
    (result, latest_hashes)
}

#[allow(clippy::too_many_lines)]
fn build_with_output_state_core(
    options: BuildOptions,
    cache: &CacheSet,
    prepared_plugins: &PreparedPlugins,
    previous_hashes: Option<&HashMap<String, String>>,
    watch_data_sink: Option<&Mutex<WatchData>>,
) -> BuildResult {
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let log_path_style = internal_path_style(options.abs_paths, AbsPaths::LOG);
    let target_features = validate_target_features(
        &log,
        options.target,
        &options.engines,
        &options.supported,
        options.platform,
    );
    let bundle = options.bundle;
    let write = options.write;
    let write_to_stdout = write && options.outdir.is_empty() && options.outfile.is_empty();
    let allow_overwrite = options.allow_overwrite;
    let file_system: Arc<dyn Fs> = match real_fs(RealFsOptions {
        abs_working_dir: options.abs_working_dir.clone(),
        want_watch_data: watch_data_sink.is_some(),
        ..RealFsOptions::default()
    }) {
        Ok(file_system) => file_system.into(),
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
    let _plugin_resolve_fs_guard = enter_plugin_resolve_fs(prepared_plugins, file_system.clone());
    let _watch_data_recorder = watch_data_sink.map(|sink| WatchDataRecorder {
        file_system: file_system.as_ref(),
        sink,
    });
    let mut plugins_after_start = prepared_plugins.plugins.clone();
    bundler::run_on_start_plugins(&log, file_system.as_ref(), &plugins_after_start);
    for plugin in &mut plugins_after_start {
        plugin.on_start.clear();
    }
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
            let (errors, warnings) = public_messages_with_path_style(log.done(), log_path_style);
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
        let (errors, warnings) = public_messages_with_path_style(log.done(), log_path_style);
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
        original_target_environment: target_features.original_target_environment,
        unsupported_js_features: target_features.unsupported_js_features,
        unsupported_css_features: target_features.unsupported_css_features,
        css_prefix_data: target_features.css_prefix_data,
        unsupported_js_feature_overrides: target_features.unsupported_js_feature_overrides,
        unsupported_js_feature_overrides_mask: target_features
            .unsupported_js_feature_overrides_mask,
        unsupported_css_feature_overrides: target_features.unsupported_css_feature_overrides,
        unsupported_css_feature_overrides_mask: target_features
            .unsupported_css_feature_overrides_mask,
        source_map: match options.sourcemap {
            BuildSourceMap::None => config::SourceMap::None,
            BuildSourceMap::Linked => config::SourceMap::LinkedWithComment,
            BuildSourceMap::External => config::SourceMap::ExternalWithoutComment,
            BuildSourceMap::Inline => config::SourceMap::Inline,
            BuildSourceMap::InlineAndExternal => config::SourceMap::InlineAndExternal,
        },
        log_path_style,
        code_path_style: internal_path_style(options.abs_paths, AbsPaths::CODE),
        metafile_path_style: internal_path_style(options.abs_paths, AbsPaths::METAFILE),
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
        watch_mode: watch_data_sink.is_some(),
        plugins: plugins_after_start,
        ..config::Options::default()
    };
    let mut entry_points: Vec<_> = options
        .entry_points
        .into_iter()
        .map(|input_path| bundler::EntryPoint {
            input_path,
            ..bundler::EntryPoint::default()
        })
        .collect();
    entry_points.extend(options.entry_points_advanced.into_iter().map(|entry| {
        bundler::EntryPoint {
            input_path: entry.input_path,
            output_path: entry.output_path,
            ..bundler::EntryPoint::default()
        }
    }));
    let compiled = bundler::bundle_javascript(
        &log,
        file_system.as_ref(),
        cache,
        &entry_points,
        &mut internal_options,
        "API",
    );
    let (mut errors, warnings) = public_messages_with_path_style(log.done(), log_path_style);
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
    if write && (errors.is_empty() || (previous_hashes.is_some() && !write_to_stdout)) {
        errors.extend(write_build_output_files(
            &output_files,
            write_to_stdout,
            &canonical_input_paths,
            allow_overwrite,
            previous_hashes,
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
    let log_path_style = internal_path_style(options.abs_paths, AbsPaths::LOG);
    let target_features = validate_target_features(
        &log,
        options.target,
        &options.engines,
        &options.supported,
        options.platform,
    );
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
    if !options.global_name.is_empty() {
        let (_, ok) = js_parser::parse_global_name(
            log.clone(),
            Source {
                key_path: crate::internal::logger::Path {
                    text: "(global name)".into(),
                    ..crate::internal::logger::Path::default()
                },
                pretty_paths: PrettyPaths {
                    abs: "(global name)".into(),
                    rel: "(global name)".into(),
                },
                contents: Arc::from(options.global_name.as_bytes()),
                ..Source::default()
            },
        );
        if !ok {
            let (errors, warnings) = public_messages_with_path_style(log.done(), log_path_style);
            return TransformResult {
                errors,
                warnings,
                ..TransformResult::default()
            };
        }
    }
    if log.has_errors() {
        let (errors, warnings) = public_messages_with_path_style(log.done(), log_path_style);
        return TransformResult {
            errors,
            warnings,
            ..TransformResult::default()
        };
    }
    let needs_tree_shaking_linker = options.tree_shaking == BuildTreeShaking::Enabled
        && matches!(
            options.loader,
            Loader::Js | Loader::Jsx | Loader::Ts | Loader::Tsx | Loader::None
        );
    if options.format != BuildFormat::Default || needs_tree_shaking_linker {
        return transform_with_linker(input.as_ref(), options);
    }
    let input_contents = Arc::<[u8]>::from(input.as_ref());
    let source = Source {
        key_path: Path {
            text: sourcefile.clone(),
            ..Path::default()
        },
        pretty_paths: PrettyPaths {
            abs: sourcefile.clone(),
            rel: sourcefile.clone(),
        },
        identifier_name: generate_non_unique_name_from_path(&sourcefile),
        contents: input_contents.clone(),
        ..Source::default()
    };

    let mut printed = match options.loader {
        Loader::Css | Loader::GlobalCss | Loader::LocalCss => {
            transform_css(&log, source, &options, &target_features)
        }
        Loader::Js | Loader::Jsx | Loader::Ts | Loader::Tsx | Loader::None => {
            transform_javascript(&log, source, &options, &target_features)
        }
        Loader::Json => TransformPrint {
            code: transform_json(&log, source, &options, &target_features),
            ..TransformPrint::default()
        },
        Loader::Text => TransformPrint {
            code: transform_text(&source, &options, &target_features),
            ..TransformPrint::default()
        },
        Loader::Base64 => TransformPrint {
            code: transform_base64(&source, &options, &target_features),
            ..TransformPrint::default()
        },
        Loader::Binary => TransformPrint {
            code: transform_binary(&source, &options, &target_features),
            ..TransformPrint::default()
        },
        Loader::DataUrl => TransformPrint {
            code: transform_data_url(&source, &options, &target_features),
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
    let (errors, warnings) = public_messages_with_path_style(messages, log_path_style);
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

fn transform_with_linker(input: &[u8], options: TransformOptions) -> TransformResult {
    let Ok(input) = String::from_utf8(input.to_vec()) else {
        return TransformResult {
            errors: vec![Message {
                text: "Formatted transform input must be valid UTF-8".into(),
                kind: MessageKind::Error,
                ..Message::default()
            }],
            ..TransformResult::default()
        };
    };
    let output_file = if options.sourcefile.is_empty() {
        "<stdin>-out".to_string()
    } else {
        format!("{}-out", options.sourcefile)
    };
    let is_css = matches!(
        options.loader,
        Loader::Css | Loader::GlobalCss | Loader::LocalCss
    );
    let (banner, footer, css_banner, css_footer) = if is_css {
        (String::new(), String::new(), options.banner, options.footer)
    } else {
        (options.banner, options.footer, String::new(), String::new())
    };
    // Transform is intentionally isolated from project configuration. A
    // non-empty raw config prevents the build scanner from walking the real
    // file system for a nearby tsconfig.json.
    let tsconfig_raw = if options.tsconfig_raw.is_empty() {
        "{}".into()
    } else {
        options.tsconfig_raw
    };
    let result = build(BuildOptions {
        stdin: Some(BuildStdin {
            contents: input,
            sourcefile: options.sourcefile,
            loader: options.loader,
            ..BuildStdin::default()
        }),
        outfile: output_file,
        abs_paths: options.abs_paths,
        format: options.format,
        platform: options.platform,
        target: options.target,
        engines: options.engines,
        supported: options.supported,
        global_name: options.global_name,
        sourcemap: options.sourcemap,
        source_root: options.source_root,
        sources_content: options.sources_content,
        legal_comments: options.legal_comments,
        line_limit: options.line_limit,
        tree_shaking: options.tree_shaking,
        jsx: options.jsx,
        jsx_factory: options.jsx_factory,
        jsx_fragment: options.jsx_fragment,
        jsx_import_source: options.jsx_import_source,
        jsx_development: options.jsx_development,
        jsx_side_effects: options.jsx_side_effects,
        minify_whitespace: options.minify_whitespace,
        minify_identifiers: options.minify_identifiers,
        minify_syntax: options.minify_syntax,
        ascii_only: options.ascii_only,
        drop_console: options.drop_console,
        drop_debugger: options.drop_debugger,
        drop_labels: options.drop_labels,
        ignore_annotations: options.ignore_annotations,
        banner,
        footer,
        css_banner,
        css_footer,
        define: options.define,
        pure: options.pure,
        keep_names: options.keep_names,
        tsconfig_raw,
        ..BuildOptions::default()
    });
    let mut transformed = TransformResult {
        errors: result.errors,
        warnings: result.warnings,
        ..TransformResult::default()
    };
    for output in result.output_files {
        if output.path.ends_with(".LEGAL.txt") {
            transformed.legal_comments = output.contents;
        } else if std::path::Path::new(&output.path)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("map"))
        {
            transformed.map = output.contents;
        } else {
            transformed.code = output.contents;
        }
    }
    transformed
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

fn transform_json(
    log: &Log,
    source: Source,
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> Vec<u8> {
    let (expression, ok) = js_parser::parse_json(
        log.clone(),
        source,
        js_parser::JsonOptions {
            unsupported_js_features: target_features.unsupported_js_features,
            ..js_parser::JsonOptions::default()
        },
    );
    if !ok {
        return Vec::new();
    }
    let renamer = new_no_op_renamer(SymbolMap::new(1));
    let value = js_printer::print_expr(
        &expression,
        &renamer,
        js_printer_options(options, target_features),
    );
    export_default(value, options.minify_whitespace)
}

fn transform_text(
    source: &Source,
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> Vec<u8> {
    let contents = source
        .contents
        .strip_prefix(&[0xef, 0xbb, 0xbf])
        .unwrap_or(&source.contents);
    export_string(contents, options, target_features)
}

fn transform_base64(
    source: &Source,
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> Vec<u8> {
    export_string(
        STANDARD.encode(&source.contents).as_bytes(),
        options,
        target_features,
    )
}

fn transform_binary(
    source: &Source,
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> Vec<u8> {
    let encoded = STANDARD.encode(&source.contents);
    let mut value = b"Uint8Array.fromBase64(".to_vec();
    value.extend(js_printer::quote_utf16(
        &string_to_utf16(encoded.as_bytes()),
        js_printer_options(options, target_features),
        true,
    ));
    value.push(b')');
    export_default(value, options.minify_whitespace)
}

fn transform_data_url(
    source: &Source,
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> Vec<u8> {
    let mime_type = guess_mime_type(&source.pretty_paths.abs, &source.contents);
    let url = encode_string_as_shortest_data_url(&mime_type, &source.contents);
    export_string(url.as_bytes(), options, target_features)
}

fn export_string(
    value: &[u8],
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> Vec<u8> {
    let quoted = js_printer::quote_utf16(
        &string_to_utf16(value),
        js_printer_options(options, target_features),
        true,
    );
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

fn js_printer_options(
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> js_printer::Options {
    js_printer::Options {
        unsupported_features: target_features.unsupported_js_features,
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

fn transform_javascript(
    log: &Log,
    source: Source,
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> TransformPrint {
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
    parser_options
        .original_target_env
        .clone_from(&target_features.original_target_environment);
    parser_options.unsupported_js_features = target_features.unsupported_js_features;
    parser_options.unsupported_js_feature_overrides =
        target_features.unsupported_js_feature_overrides;
    parser_options.unsupported_js_feature_overrides_mask =
        target_features.unsupported_js_feature_overrides_mask;
    parser_options.log_path_style = internal_path_style(options.abs_paths, AbsPaths::LOG);
    parser_options.code_path_style = internal_path_style(options.abs_paths, AbsPaths::CODE);
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
    let (renamer, helpers) = transform_runtime_renamer(
        &ast,
        symbols,
        options.keep_names,
        options.minify_identifiers,
    );
    let printed = if let Some(line_offset_tables) = line_offset_tables {
        js_printer::print_with_source_map(
            &ast,
            &renamer,
            js_printer_options(options, target_features),
            None,
            line_offset_tables,
        )
    } else {
        js_printer::print(&ast, &renamer, js_printer_options(options, target_features))
    };
    let mut code = printed.js;
    let printed_len = code.len();
    prepend_transform_runtime_helpers(&mut code, &helpers, options.minify_whitespace);
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

fn transform_css(
    log: &Log,
    source: Source,
    options: &TransformOptions,
    target_features: &ValidatedTargetFeatures,
) -> TransformPrint {
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
            unsupported_css_features: target_features.unsupported_css_features,
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

fn public_messages_with_path_style(
    messages: Vec<Msg>,
    path_style: PathStyle,
) -> (Vec<Message>, Vec<Message>) {
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
            location: message
                .data
                .location
                .map(|location| public_location(location, path_style)),
            notes: message
                .notes
                .into_iter()
                .map(|note| Note {
                    text: note.text,
                    location: note
                        .location
                        .map(|location| public_location(location, path_style)),
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

fn public_location(location: MsgLocation, path_style: PathStyle) -> Location {
    Location {
        file: location.file.select(path_style).to_string(),
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
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use super::{
        AbsPaths, BuildEntryPoint, BuildFormat, BuildJsx, BuildLegalComments, BuildOptions,
        BuildPlatform, BuildSourceMap, BuildSourcesContent, BuildStdin, BuildTreeShaking,
        ContextError, Engine, EngineName, Loader, OnLoadOptions, OnLoadResult, OnResolveOptions,
        OnResolveResult, Packages, Plugin, PluginError, ResolveKind, SideEffects, Target,
        TransformOptions, WatchOptions, build as build_api, context, transform,
    };

    fn build(mut options: BuildOptions) -> super::BuildResult {
        options.bundle = true;
        build_api(options)
    }

    fn code(result: super::TransformResult) -> String {
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        String::from_utf8(result.code).expect("transform output is UTF-8")
    }

    fn transform_code(source: &str, options: TransformOptions) -> String {
        code(transform(source, options))
    }

    fn assert_api_error(
        message: &super::Message,
        text: &str,
        line: usize,
        column: usize,
        length: usize,
    ) {
        assert_eq!(message.text, text);
        let location = message.location.as_ref().expect("API error location");
        assert_eq!(
            (location.line, location.column, location.length),
            (line, column, length)
        );
    }

    fn assert_transform_error(
        result: &super::TransformResult,
        text: &str,
        column: usize,
        length: usize,
    ) {
        assert!(result.code.is_empty());
        assert!(result.warnings.is_empty());
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert_api_error(&result.errors[0], text, 1, column, length);
    }

    fn context_test_directory(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-context-{name}-{unique}"));
        std::fs::create_dir_all(&directory).expect("create context test directory");
        directory
    }

    fn context_options(directory: &std::path::Path) -> BuildOptions {
        BuildOptions {
            bundle: true,
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            ..BuildOptions::default()
        }
    }

    fn wait_for_context_change(description: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while !predicate() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn public_plugins_build_virtual_modules_and_convert_callback_data() {
        let directory = context_test_directory("plugin-build");
        std::fs::write(directory.join("dep.js"), "export const value = 42")
            .expect("write plugin resolve-dir dependency");
        let setup_count = Arc::new(AtomicUsize::new(0));
        let saw_item_resolve = Arc::new(AtomicBool::new(false));
        let saw_item_load = Arc::new(AtomicBool::new(false));
        let plugin_data: super::PluginData = Arc::new("from-resolve".to_string());
        let plugin = Plugin::new("virtual", {
            let setup_count = setup_count.clone();
            let saw_item_resolve = saw_item_resolve.clone();
            let saw_item_load = saw_item_load.clone();
            let plugin_data = plugin_data.clone();
            move |plugin_build| {
                setup_count.fetch_add(1, Ordering::SeqCst);
                plugin_build.initial_options.bundle = true;
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^virtual-entry$".into(),
                        ..OnResolveOptions::default()
                    },
                    |args| {
                        assert_eq!(args.kind, ResolveKind::EntryPoint);
                        assert!(args.importer.is_empty());
                        assert!(args.namespace.is_empty());
                        Ok(OnResolveResult {
                            path: "entry".into(),
                            namespace: "virtual".into(),
                            ..OnResolveResult::default()
                        })
                    },
                );
                plugin_build.on_load(
                    OnLoadOptions {
                        filter: "^entry$".into(),
                        namespace: "virtual".into(),
                    },
                    |_| {
                        Ok(OnLoadResult {
                            contents: Some(
                                "import value from 'virtual:item' with { type: 'custom' }; console.log(value)"
                                    .into(),
                            ),
                            loader: Loader::Js,
                            ..OnLoadResult::default()
                        })
                    },
                );
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^virtual:item$".into(),
                        namespace: "virtual".into(),
                    },
                    {
                        let saw_item_resolve = saw_item_resolve.clone();
                        let plugin_data = plugin_data.clone();
                        move |args| {
                            saw_item_resolve.store(
                                args.kind == ResolveKind::ImportStatement
                                    && args.namespace == "virtual"
                                    && args.with.get("type").is_some_and(|value| value == "custom"),
                                Ordering::SeqCst,
                            );
                            Ok(OnResolveResult {
                                path: "item".into(),
                                namespace: "virtual".into(),
                                suffix: "?raw".into(),
                                side_effects: SideEffects::False,
                                plugin_data: Some(plugin_data.clone()),
                                ..OnResolveResult::default()
                            })
                        }
                    },
                );
                plugin_build.on_load(
                    OnLoadOptions {
                        filter: "^item$".into(),
                        namespace: "virtual".into(),
                    },
                    {
                        let saw_item_load = saw_item_load.clone();
                        move |args| {
                            let data = args
                                .plugin_data
                                .as_deref()
                                .and_then(|data| data.downcast_ref::<String>());
                            saw_item_load.store(
                                args.suffix == "?raw"
                                    && data.is_some_and(|value| value == "from-resolve")
                                    && args.with.get("type").is_some_and(|value| value == "custom"),
                                Ordering::SeqCst,
                            );
                            Ok(OnLoadResult {
                                contents: Some(
                                    "import {value} from './dep.js'; export default value".into(),
                                ),
                                resolve_dir: ".".into(),
                                loader: Loader::Js,
                                ..OnLoadResult::default()
                            })
                        }
                    },
                );
                Ok(())
            }
        });

        let result = build_api(BuildOptions {
            entry_points: vec!["virtual-entry".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![plugin],
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(setup_count.load(Ordering::SeqCst), 1);
        assert!(saw_item_resolve.load(Ordering::SeqCst));
        assert!(saw_item_load.load(Ordering::SeqCst));
        assert_eq!(result.output_files.len(), 1);
        assert!(
            result.output_files[0]
                .path
                .ends_with("/out/virtual-entry.js")
        );
        assert!(String::from_utf8_lossy(&result.output_files[0].contents).contains("42"));
        std::fs::remove_dir_all(directory).expect("remove plugin build directory");
    }

    #[test]
    fn public_plugin_setup_runs_once_across_context_rebuilds() {
        let directory = context_test_directory("plugin-context");
        let setup_count = Arc::new(AtomicUsize::new(0));
        let contents = Arc::new(Mutex::new("console.log('first')".to_string()));
        let plugin = Plugin::new("context-plugin", {
            let setup_count = setup_count.clone();
            let contents = contents.clone();
            move |plugin_build| {
                setup_count.fetch_add(1, Ordering::SeqCst);
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^virtual-context$".into(),
                        ..OnResolveOptions::default()
                    },
                    |_| {
                        Ok(OnResolveResult {
                            path: "context".into(),
                            namespace: "virtual".into(),
                            ..OnResolveResult::default()
                        })
                    },
                );
                plugin_build.on_load(
                    OnLoadOptions {
                        filter: "^context$".into(),
                        namespace: "virtual".into(),
                    },
                    {
                        let contents = contents.clone();
                        move |_| {
                            Ok(OnLoadResult {
                                contents: Some(
                                    contents
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .clone(),
                                ),
                                loader: Loader::Js,
                                ..OnLoadResult::default()
                            })
                        }
                    },
                );
                Ok(())
            }
        });
        let build_context = context(BuildOptions {
            bundle: true,
            entry_points: vec!["virtual-context".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![plugin],
            ..BuildOptions::default()
        })
        .expect("create plugin context");
        assert_eq!(setup_count.load(Ordering::SeqCst), 1);

        let first = build_context.rebuild();
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert!(String::from_utf8_lossy(&first.output_files[0].contents).contains("first"));
        *contents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = "console.log('second')".into();
        let second = build_context.rebuild();
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        assert!(String::from_utf8_lossy(&second.output_files[0].contents).contains("second"));
        assert_eq!(setup_count.load(Ordering::SeqCst), 1);

        build_context.dispose();
        std::fs::remove_dir_all(directory).expect("remove plugin context directory");
    }

    #[test]
    fn public_plugin_resolve_enforces_setup_kind_and_build_lifetimes() {
        let retained_resolve = Arc::new(Mutex::new(None::<super::ResolveCallback>));
        let plugin = Plugin::new("resolve-lifecycle", {
            let retained_resolve = retained_resolve.clone();
            move |plugin_build| {
                let early = (plugin_build.resolve)(
                    "early",
                    super::ResolveOptions {
                        kind: ResolveKind::EntryPoint,
                        ..super::ResolveOptions::default()
                    },
                );
                assert_eq!(early.errors.len(), 1);
                assert_eq!(
                    early.errors[0].text,
                    "Cannot call \"resolve\" before plugin setup has completed"
                );
                let resolve = plugin_build.resolve.clone();
                *retained_resolve
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(resolve.clone());
                plugin_build.on_start(move || {
                    let bad_kind = resolve("bad-kind", super::ResolveOptions::default());
                    assert_eq!(bad_kind.errors.len(), 1);
                    assert_eq!(
                        bad_kind.errors[0].text,
                        "Must specify \"kind\" when calling \"resolve\""
                    );
                    let missing = resolve(
                        "missing-package",
                        super::ResolveOptions {
                            plugin_name: "override-name".into(),
                            kind: ResolveKind::ImportStatement,
                            ..super::ResolveOptions::default()
                        },
                    );
                    assert_eq!(missing.errors.len(), 1);
                    assert!(missing.errors[0].plugin_name.is_empty());
                    assert_eq!(missing.errors[0].notes.len(), 1);
                    assert_eq!(
                        missing.errors[0].notes[0].text,
                        "The plugin \"override-name\" didn't set a resolve directory, so esbuild \
                         did not search for \"missing-package\" on the file system."
                    );
                    Ok(super::OnStartResult::default())
                });
                Ok(())
            }
        });
        let result = build_api(BuildOptions {
            plugins: vec![plugin],
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        let inactive = retained_resolve
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("capture resolve callback")(
            "late",
            super::ResolveOptions {
                kind: ResolveKind::EntryPoint,
                ..super::ResolveOptions::default()
            },
        );
        assert_eq!(inactive.errors.len(), 1);
        assert_eq!(
            inactive.errors[0].text,
            "Cannot call \"resolve\" on an inactive build"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn public_plugin_resolve_runs_builtin_and_nested_plugin_resolution() {
        let directory = context_test_directory("plugin-resolve");
        let source_directory = directory.join("src");
        std::fs::create_dir_all(&source_directory).expect("create resolve source directory");
        let input_path = source_directory.join("input.js");
        std::fs::write(&input_path, "console.log(123)").expect("write resolve input");
        std::fs::write(source_directory.join("dep.custom"), "export default 1")
            .expect("write custom-extension dependency");
        let input_plugin_data: super::PluginData = Arc::new("input-data".to_string());
        let output_plugin_data: super::PluginData = Arc::new("output-data".to_string());
        let saw_nested = Arc::new(AtomicBool::new(false));
        let plugin = Plugin::new("resolve-chain", {
            let directory = directory.clone();
            let source_directory = source_directory.clone();
            let input_path = input_path.clone();
            let input_plugin_data = input_plugin_data.clone();
            let output_plugin_data = output_plugin_data.clone();
            let saw_nested = saw_nested.clone();
            move |plugin_build| {
                plugin_build.initial_options.resolve_extensions = vec![".custom".into()];
                plugin_build
                    .initial_options
                    .external
                    .push("external-pkg".into());
                let resolve = plugin_build.resolve.clone();
                plugin_build.on_start({
                    let resolve = resolve.clone();
                    let source_directory = source_directory.clone();
                    let expected = std::fs::canonicalize(source_directory.join("dep.custom"))
                        .expect("canonicalize custom-extension dependency");
                    move || {
                        let result = resolve(
                            "./dep",
                            super::ResolveOptions {
                                resolve_dir: source_directory.to_string_lossy().into_owned(),
                                kind: ResolveKind::ImportStatement,
                                ..super::ResolveOptions::default()
                            },
                        );
                        assert!(result.errors.is_empty(), "{:?}", result.errors);
                        assert_eq!(std::path::Path::new(&result.path), expected);
                        assert_eq!(result.namespace, "file");
                        assert!(result.side_effects);
                        let external = resolve(
                            "external-pkg",
                            super::ResolveOptions {
                                resolve_dir: source_directory.to_string_lossy().into_owned(),
                                kind: ResolveKind::ImportStatement,
                                ..super::ResolveOptions::default()
                            },
                        );
                        assert!(external.errors.is_empty(), "{:?}", external.errors);
                        assert_eq!(external.path, "external-pkg");
                        assert!(external.external);
                        let missing = resolve(
                            "./missing",
                            super::ResolveOptions {
                                resolve_dir: source_directory.to_string_lossy().into_owned(),
                                kind: ResolveKind::ImportStatement,
                                ..super::ResolveOptions::default()
                            },
                        );
                        assert_eq!(missing.errors.len(), 1);
                        assert_eq!(missing.errors[0].text, "Could not resolve \"./missing\"");
                        Ok(super::OnStartResult::default())
                    }
                });
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^entry$".into(),
                        ..OnResolveOptions::default()
                    },
                    {
                        let resolve = resolve.clone();
                        let directory = directory.clone();
                        let input_path = input_path.clone();
                        let input_plugin_data = input_plugin_data.clone();
                        move |_| {
                            let result = resolve(
                                "foo",
                                super::ResolveOptions {
                                    importer: "foo-importer".into(),
                                    namespace: "foo-namespace".into(),
                                    resolve_dir: "foo-resolve-dir".into(),
                                    kind: ResolveKind::DynamicImport,
                                    plugin_data: Some(input_plugin_data.clone()),
                                    with: HashMap::from([("type".into(), "custom".into())]),
                                    ..super::ResolveOptions::default()
                                },
                            );
                            assert!(result.errors.is_empty(), "{:?}", result.errors);
                            assert_eq!(std::path::Path::new(&result.path), input_path);
                            assert_eq!(result.namespace, "file");
                            assert_eq!(result.suffix, "?nested");
                            assert!(!result.external);
                            assert!(!result.side_effects);
                            assert_eq!(result.warnings.len(), 1);
                            let data = result
                                .plugin_data
                                .as_deref()
                                .and_then(|value| value.downcast_ref::<String>());
                            assert_eq!(data.map(String::as_str), Some("output-data"));
                            Ok(OnResolveResult {
                                path: result.path,
                                external: result.external,
                                side_effects: if result.side_effects {
                                    SideEffects::True
                                } else {
                                    SideEffects::False
                                },
                                namespace: result.namespace,
                                suffix: result.suffix,
                                plugin_data: result.plugin_data,
                                warnings: result.warnings,
                                watch_dirs: vec![directory.to_string_lossy().into_owned()],
                                ..OnResolveResult::default()
                            })
                        }
                    },
                );
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^foo$".into(),
                        ..OnResolveOptions::default()
                    },
                    {
                        let directory = directory.clone();
                        let input_path = input_path.clone();
                        let output_plugin_data = output_plugin_data.clone();
                        let saw_nested = saw_nested.clone();
                        move |args| {
                            let data = args
                                .plugin_data
                                .as_deref()
                                .and_then(|value| value.downcast_ref::<String>());
                            assert_eq!(args.importer, "foo-importer");
                            assert_eq!(args.namespace, "foo-namespace");
                            assert_eq!(
                                args.resolve_dir,
                                std::fs::canonicalize(&directory)
                                    .expect("canonicalize resolve working directory")
                                    .join("foo-resolve-dir")
                                    .to_string_lossy()
                                    .into_owned()
                            );
                            assert_eq!(args.kind, ResolveKind::DynamicImport);
                            assert_eq!(args.with.get("type").map(String::as_str), Some("custom"));
                            assert_eq!(data.map(String::as_str), Some("input-data"));
                            saw_nested.store(true, Ordering::SeqCst);
                            Ok(OnResolveResult {
                                path: input_path.to_string_lossy().into_owned(),
                                suffix: "?nested".into(),
                                side_effects: SideEffects::False,
                                plugin_data: Some(output_plugin_data.clone()),
                                warnings: vec![super::Message {
                                    text: "nested warning".into(),
                                    ..super::Message::default()
                                }],
                                ..OnResolveResult::default()
                            })
                        }
                    },
                );
                Ok(())
            }
        });

        let result = build_api(BuildOptions {
            bundle: true,
            entry_points: vec!["entry".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![plugin],
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(saw_nested.load(Ordering::SeqCst));
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0].text, "nested warning");
        assert_eq!(result.output_files.len(), 1);
        assert!(String::from_utf8_lossy(&result.output_files[0].contents).contains("123"));
        std::fs::remove_dir_all(directory).expect("remove plugin resolve directory");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn public_plugin_lifecycle_hooks_run_for_each_rebuild_and_dispose_once() {
        let directory = context_test_directory("plugin-lifecycle");
        let setup_count = Arc::new(AtomicUsize::new(0));
        let start_count = Arc::new(AtomicUsize::new(0));
        let end_count = Arc::new(AtomicUsize::new(0));
        let dispose_count = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let plugin = Plugin::new("lifecycle", {
            let setup_count = setup_count.clone();
            let start_count = start_count.clone();
            let end_count = end_count.clone();
            let dispose_count = dispose_count.clone();
            let started = started.clone();
            move |plugin_build| {
                setup_count.fetch_add(1, Ordering::SeqCst);
                plugin_build.on_start({
                    let start_count = start_count.clone();
                    let started = started.clone();
                    move || {
                        start_count.fetch_add(1, Ordering::SeqCst);
                        started.store(true, Ordering::SeqCst);
                        Ok(super::OnStartResult::default())
                    }
                });
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^virtual-lifecycle$".into(),
                        ..OnResolveOptions::default()
                    },
                    |_| {
                        Ok(OnResolveResult {
                            path: "lifecycle".into(),
                            namespace: "virtual".into(),
                            ..OnResolveResult::default()
                        })
                    },
                );
                plugin_build.on_load(
                    OnLoadOptions {
                        filter: "^lifecycle$".into(),
                        namespace: "virtual".into(),
                    },
                    {
                        let started = started.clone();
                        move |_| {
                            assert!(
                                started.swap(false, Ordering::SeqCst),
                                "onStart must finish before onLoad"
                            );
                            Ok(OnLoadResult {
                                contents: Some("console.log('lifecycle')".into()),
                                loader: Loader::Js,
                                ..OnLoadResult::default()
                            })
                        }
                    },
                );
                plugin_build.on_end({
                    let end_count = end_count.clone();
                    move |result| {
                        assert!(result.errors.is_empty(), "{:?}", result.errors);
                        assert_eq!(result.output_files.len(), 1);
                        end_count.fetch_add(1, Ordering::SeqCst);
                        Ok(super::OnEndResult {
                            warnings: vec![super::Message {
                                text: "ended".into(),
                                ..super::Message::default()
                            }],
                            ..super::OnEndResult::default()
                        })
                    }
                });
                plugin_build.on_dispose({
                    let dispose_count = dispose_count.clone();
                    move || {
                        dispose_count.fetch_add(1, Ordering::SeqCst);
                    }
                });
                Ok(())
            }
        });
        let build_context = context(BuildOptions {
            bundle: true,
            entry_points: vec!["virtual-lifecycle".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![plugin],
            ..BuildOptions::default()
        })
        .expect("create lifecycle context");

        for _ in 0..2 {
            let result = build_context.rebuild();
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            assert_eq!(result.warnings.len(), 1);
            assert_eq!(result.warnings[0].plugin_name, "lifecycle");
            assert_eq!(result.warnings[0].text, "ended");
            assert_eq!(result.warnings[0].kind, super::MessageKind::Warning);
        }
        assert_eq!(setup_count.load(Ordering::SeqCst), 1);
        assert_eq!(start_count.load(Ordering::SeqCst), 2);
        assert_eq!(end_count.load(Ordering::SeqCst), 2);

        build_context.dispose();
        wait_for_context_change("plugin dispose callback", || {
            dispose_count.load(Ordering::SeqCst) == 1
        });
        build_context.dispose();
        assert_eq!(dispose_count.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(directory).expect("remove lifecycle context directory");
    }

    #[test]
    fn public_plugin_one_shot_runs_end_and_dispose_after_start_errors() {
        let directory = context_test_directory("plugin-lifecycle-error");
        std::fs::write(directory.join("entry.js"), "console.log('entry')")
            .expect("write lifecycle error entry");
        let end_count = Arc::new(AtomicUsize::new(0));
        let dispose_count = Arc::new(AtomicUsize::new(0));
        let plugin = Plugin::new("lifecycle-error", {
            let end_count = end_count.clone();
            let dispose_count = dispose_count.clone();
            move |plugin_build| {
                plugin_build.on_start(|| Err(PluginError::new("start failed")));
                plugin_build.on_end({
                    let end_count = end_count.clone();
                    move |result| {
                        assert!(
                            result
                                .errors
                                .iter()
                                .any(|error| error.text == "start failed")
                        );
                        end_count.fetch_add(1, Ordering::SeqCst);
                        Ok(super::OnEndResult {
                            warnings: vec![super::Message {
                                text: "end still ran".into(),
                                ..super::Message::default()
                            }],
                            ..super::OnEndResult::default()
                        })
                    }
                });
                plugin_build.on_dispose({
                    let dispose_count = dispose_count.clone();
                    move || {
                        dispose_count.fetch_add(1, Ordering::SeqCst);
                    }
                });
                Ok(())
            }
        });
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            write: true,
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![plugin],
            ..BuildOptions::default()
        });
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].plugin_name, "lifecycle-error");
        assert_eq!(result.errors[0].text, "start failed");
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0].plugin_name, "lifecycle-error");
        assert_eq!(result.warnings[0].kind, super::MessageKind::Warning);
        assert!(result.output_files.is_empty());
        assert!(
            !directory.join("out").join("entry.js").exists(),
            "an onStart error must suppress generated and written output"
        );
        assert_eq!(end_count.load(Ordering::SeqCst), 1);
        wait_for_context_change("one-shot plugin dispose callback", || {
            dispose_count.load(Ordering::SeqCst) == 1
        });
        std::fs::remove_dir_all(directory).expect("remove lifecycle error directory");
    }

    #[test]
    fn public_plugin_on_end_runs_for_early_errors_and_cannot_change_rebuild_hashes() {
        let directory = context_test_directory("plugin-on-end-output-state");
        std::fs::write(directory.join("entry.js"), "console.log('entry')")
            .expect("write onEnd output-state entry");
        let build_context = context(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            write: false,
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![Plugin::new("output-state", |plugin_build| {
                plugin_build.on_end(|result| {
                    result.output_files.clear();
                    Ok(super::OnEndResult::default())
                });
                Ok(())
            })],
            ..BuildOptions::default()
        })
        .expect("create output-state context");
        let result = build_context.rebuild();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.output_files.is_empty());
        assert_eq!(
            build_context
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .latest_hashes
                .len(),
            1,
            "rebuild bookkeeping must snapshot output hashes before onEnd mutates the result"
        );
        build_context.dispose();
        std::fs::remove_dir_all(&directory).expect("remove output-state context directory");

        let directory = context_test_directory("plugin-on-end-early-error");
        let end_count = Arc::new(AtomicUsize::new(0));
        let prepared_plugins = super::PreparedPlugins {
            on_end: vec![super::PreparedOnEnd {
                plugin_name: "early-error".into(),
                callback: {
                    let end_count = end_count.clone();
                    Arc::new(move |result| {
                        assert!(!result.errors.is_empty());
                        end_count.fetch_add(1, Ordering::SeqCst);
                        Ok(super::OnEndResult::default())
                    })
                },
            }],
            ..super::PreparedPlugins::default()
        };
        let (result, _) = super::build_with_output_state(
            BuildOptions {
                entry_points: vec!["first.js".into(), "second.js".into()],
                abs_working_dir: directory.to_string_lossy().into_owned(),
                ..BuildOptions::default()
            },
            &super::CacheSet::default(),
            &prepared_plugins,
            None,
            None,
        );
        assert_eq!(result.errors.len(), 1);
        assert_eq!(
            result.errors[0].text,
            "Must use \"outdir\" when there are multiple input files"
        );
        assert_eq!(end_count.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(&directory).expect("remove early-error working directory");
    }

    #[test]
    fn public_plugin_setup_validation_is_controlled_and_attributed() {
        let missing_setup_count = Arc::new(AtomicUsize::new(0));
        let missing_name = build_api(BuildOptions {
            plugins: vec![Plugin::new("", {
                let missing_setup_count = missing_setup_count.clone();
                move |_| {
                    missing_setup_count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })],
            ..BuildOptions::default()
        });
        assert_eq!(missing_setup_count.load(Ordering::SeqCst), 0);
        assert_eq!(
            missing_name.errors[0].text,
            "Plugin at index 0 is missing a name"
        );

        let invalid_filter = build_api(BuildOptions {
            plugins: vec![Plugin::new("invalid-filter", |plugin_build| {
                plugin_build.on_load(OnLoadOptions::default(), |_| Ok(OnLoadResult::default()));
                Ok(())
            })],
            ..BuildOptions::default()
        });
        assert_eq!(invalid_filter.errors.len(), 1);
        assert_eq!(invalid_filter.errors[0].plugin_name, "invalid-filter");
        assert!(
            invalid_filter.errors[0]
                .text
                .contains("is missing a filter")
        );

        let setup_error = build_api(BuildOptions {
            plugins: vec![Plugin::new("setup-error", |_| {
                Err(PluginError::new("setup failed"))
            })],
            ..BuildOptions::default()
        });
        assert_eq!(setup_error.errors.len(), 1);
        assert_eq!(setup_error.errors[0].plugin_name, "setup-error");
        assert_eq!(setup_error.errors[0].text, "setup failed");

        let setup_panic = build_api(BuildOptions {
            plugins: vec![Plugin::new("setup-panic", |_| {
                panic!("intentional setup panic")
            })],
            ..BuildOptions::default()
        });
        assert_eq!(setup_panic.errors.len(), 1);
        assert_eq!(setup_panic.errors[0].plugin_name, "setup-panic");
        assert_eq!(setup_panic.errors[0].text, "Plugin setup callback panicked");

        let directory = context_test_directory("plugin-working-dir");
        let working_dir_mutation = build_api(BuildOptions {
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![Plugin::new("working-dir", |plugin_build| {
                plugin_build.initial_options.abs_working_dir = "/other".into();
                Ok(())
            })],
            ..BuildOptions::default()
        });
        assert!(
            working_dir_mutation.errors.iter().any(|error| {
                error.text == "Mutating \"abs_working_dir\" during plugin setup is not allowed"
            }),
            "{:?}",
            working_dir_mutation.errors
        );
        std::fs::remove_dir_all(directory).expect("remove plugin working directory");
    }

    #[test]
    fn public_plugin_callback_errors_point_to_the_triggering_import() {
        let directory = context_test_directory("plugin-callback-error");
        std::fs::write(
            directory.join("entry.js"),
            "import value from 'virtual:error'; console.log(value)",
        )
        .expect("write plugin callback entry");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![Plugin::new("callback-error", |plugin_build| {
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^virtual:error$".into(),
                        ..OnResolveOptions::default()
                    },
                    |_| Err(PluginError::new("resolve failed")),
                );
                Ok(())
            })],
            ..BuildOptions::default()
        });
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].plugin_name, "callback-error");
        assert_eq!(result.errors[0].text, "resolve failed");
        assert_eq!(
            result.errors[0]
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some("entry.js")
        );
        std::fs::remove_dir_all(directory).expect("remove plugin callback directory");
    }

    #[test]
    fn public_plugin_message_locations_are_sanitized() {
        let directory = context_test_directory("plugin-message-location");
        std::fs::write(
            directory.join("entry.js"),
            "import value from 'virtual:location'; console.log(value)",
        )
        .expect("write plugin location entry");
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![Plugin::new("location-plugin", |plugin_build| {
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^virtual:location$".into(),
                        ..OnResolveOptions::default()
                    },
                    |_| {
                        Ok(OnResolveResult {
                            errors: vec![
                                super::Message {
                                    text: "namespaced location".into(),
                                    location: Some(super::Location {
                                        file: "file1".into(),
                                        namespace: "ns1".into(),
                                        line_text: "bad".into(),
                                        ..super::Location::default()
                                    }),
                                    notes: vec![super::Note {
                                        text: "namespaced note".into(),
                                        location: Some(super::Location {
                                            file: "note1".into(),
                                            namespace: "notes".into(),
                                            ..super::Location::default()
                                        }),
                                    }],
                                    ..super::Message::default()
                                },
                                super::Message {
                                    text: "importer fallback".into(),
                                    location: Some(super::Location::default()),
                                    ..super::Message::default()
                                },
                            ],
                            ..OnResolveResult::default()
                        })
                    },
                );
                Ok(())
            })],
            ..BuildOptions::default()
        });
        let namespaced = result
            .errors
            .iter()
            .find(|error| error.text == "namespaced location")
            .expect("namespaced plugin error");
        assert_eq!(
            namespaced
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some("ns1:file1")
        );
        assert_eq!(
            namespaced
                .notes
                .first()
                .and_then(|note| note.location.as_ref())
                .map(|location| location.file.as_str()),
            Some("notes:note1")
        );
        assert!(
            namespaced.notes.iter().any(|note| {
                note.text == "The plugin \"location-plugin\" was triggered by this import"
                    && note
                        .location
                        .as_ref()
                        .is_some_and(|location| location.file == "entry.js")
            }),
            "{:?}",
            namespaced.notes
        );
        let fallback = result
            .errors
            .iter()
            .find(|error| error.text == "importer fallback")
            .expect("fallback plugin error");
        assert_eq!(
            fallback
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some("entry.js")
        );
        std::fs::remove_dir_all(directory).expect("remove plugin location directory");
    }

    #[test]
    fn public_plugin_watch_paths_rebuild_without_rerunning_setup() {
        let directory = context_test_directory("plugin-watch");
        let watched_path = directory.join("data.txt");
        std::fs::write(&watched_path, "first").expect("write plugin watch input");
        let setup_count = Arc::new(AtomicUsize::new(0));
        let plugin = Plugin::new("watch-plugin", {
            let directory = directory.clone();
            let setup_count = setup_count.clone();
            move |plugin_build| {
                setup_count.fetch_add(1, Ordering::SeqCst);
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^virtual-watch$".into(),
                        ..OnResolveOptions::default()
                    },
                    |_| {
                        Ok(OnResolveResult {
                            path: "watch".into(),
                            namespace: "virtual".into(),
                            ..OnResolveResult::default()
                        })
                    },
                );
                plugin_build.on_load(
                    OnLoadOptions {
                        filter: "^watch$".into(),
                        namespace: "virtual".into(),
                    },
                    {
                        let directory = directory.clone();
                        move |_| {
                            let value = std::fs::read_to_string(directory.join("data.txt"))
                                .map_err(|error| PluginError::new(error.to_string()))?;
                            Ok(OnLoadResult {
                                contents: Some(format!("console.log({value:?})")),
                                loader: Loader::Js,
                                watch_files: vec!["data.txt".into()],
                                ..OnLoadResult::default()
                            })
                        }
                    },
                );
                Ok(())
            }
        });
        let build_context = context(BuildOptions {
            bundle: true,
            entry_points: vec!["virtual-watch".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            write: true,
            plugins: vec![plugin],
            ..BuildOptions::default()
        })
        .expect("create plugin watch context");
        build_context
            .watch(WatchOptions::default())
            .expect("enable plugin watch");
        let output_path = directory.join("out/virtual-watch.js");
        wait_for_context_change("initial plugin watch build", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("first"))
        });

        std::fs::write(&watched_path, "second").expect("edit plugin watch input");
        wait_for_context_change("plugin watch rebuild", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("second"))
        });
        assert_eq!(setup_count.load(Ordering::SeqCst), 1);

        build_context.dispose();
        std::fs::remove_dir_all(directory).expect("remove plugin watch directory");
    }

    #[test]
    fn public_plugin_nested_resolve_watch_paths_trigger_rebuilds() {
        let directory = context_test_directory("plugin-resolve-watch");
        std::fs::write(directory.join("entry.js"), "console.log('entry')")
            .expect("write resolve-watch entry");
        let watched_path = directory.join("resolve-watch.txt");
        std::fs::write(&watched_path, "first").expect("write nested resolve watch file");
        let start_count = Arc::new(AtomicUsize::new(0));
        let plugin = Plugin::new("resolve-watch", {
            let start_count = start_count.clone();
            move |plugin_build| {
                let resolve = plugin_build.resolve.clone();
                plugin_build.on_start({
                    let start_count = start_count.clone();
                    move || {
                        start_count.fetch_add(1, Ordering::SeqCst);
                        let result = resolve(
                            "watch-dependency",
                            super::ResolveOptions {
                                kind: ResolveKind::ImportStatement,
                                ..super::ResolveOptions::default()
                            },
                        );
                        assert!(result.errors.is_empty(), "{:?}", result.errors);
                        assert!(result.external);
                        Ok(super::OnStartResult::default())
                    }
                });
                plugin_build.on_resolve(
                    OnResolveOptions {
                        filter: "^watch-dependency$".into(),
                        ..OnResolveOptions::default()
                    },
                    |_| {
                        Ok(OnResolveResult {
                            path: "watch-dependency".into(),
                            external: true,
                            watch_files: vec!["resolve-watch.txt".into()],
                            ..OnResolveResult::default()
                        })
                    },
                );
                Ok(())
            }
        });
        let build_context = context(BuildOptions {
            bundle: true,
            entry_points: vec!["entry.js".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            plugins: vec![plugin],
            ..BuildOptions::default()
        })
        .expect("create nested resolve watch context");
        build_context
            .watch(WatchOptions::default())
            .expect("enable nested resolve watch");
        wait_for_context_change("initial nested resolve watch build", || {
            start_count.load(Ordering::SeqCst) >= 1
        });
        std::fs::write(&watched_path, "second").expect("edit nested resolve watch file");
        wait_for_context_change("nested resolve watch rebuild", || {
            start_count.load(Ordering::SeqCst) >= 2
        });

        build_context.dispose();
        std::fs::remove_dir_all(directory).expect("remove nested resolve watch directory");
    }

    #[test]
    fn validates_build_context_creation_without_scanning_entries() {
        assert_eq!(
            ContextError::default().to_string(),
            "Context creation failed"
        );

        let topology_error = context(BuildOptions {
            outfile: "out.js".into(),
            outdir: "out".into(),
            ..BuildOptions::default()
        })
        .err()
        .expect("invalid output topology");
        assert_eq!(
            topology_error
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Cannot use both \"outfile\" and \"outdir\"")
        );

        let external_error = context(BuildOptions {
            external: vec!["react".into()],
            ..BuildOptions::default()
        })
        .err()
        .expect("external requires bundling");
        assert!(
            external_error
                .errors
                .iter()
                .any(|message| message.text == "Cannot use \"external\" without \"bundle\"")
        );

        let loader_error = context(BuildOptions {
            loader: HashMap::from([("js".into(), Loader::Js)]),
            ..BuildOptions::default()
        })
        .err()
        .expect("loader extensions start with a dot");
        assert!(
            loader_error
                .errors
                .iter()
                .any(|message| message.text == "Invalid file extension: \"js\"")
        );

        let directory = context_test_directory("no-scan");
        let build_context =
            context(context_options(&directory)).expect("missing entry is valid at creation");
        std::fs::write(directory.join("entry.js"), "console.log('created later')")
            .expect("create entry after context");
        let result = build_context.rebuild();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(
            String::from_utf8_lossy(&result.output_files[0].contents).contains("created later")
        );
        std::fs::remove_dir_all(directory).expect("remove context test directory");
    }

    #[test]
    fn build_context_recovers_from_missing_files_and_syntax_errors() {
        let directory = context_test_directory("recovery");
        let build_context =
            context(context_options(&directory)).expect("create context before entry exists");

        let missing = build_context.rebuild();
        assert!(!missing.errors.is_empty());

        std::fs::write(directory.join("entry.js"), "if (").expect("write invalid entry");
        let invalid = build_context.rebuild();
        assert!(!invalid.errors.is_empty());

        std::fs::write(directory.join("entry.js"), "console.log('recovered')")
            .expect("repair entry");
        let recovered = build_context.rebuild();
        assert!(recovered.errors.is_empty(), "{:?}", recovered.errors);
        assert!(String::from_utf8_lossy(&recovered.output_files[0].contents).contains("recovered"));
        std::fs::remove_dir_all(directory).expect("remove context test directory");
    }

    #[test]
    fn build_context_rebuild_observes_edits_and_new_directory_entries() {
        let directory = context_test_directory("edits");
        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './a.js'; console.log(value)",
        )
        .expect("write entry");
        std::fs::write(directory.join("a.js"), "export const value = 'first'")
            .expect("write first dependency");
        let build_context = context(context_options(&directory)).expect("create context");

        let first = build_context.rebuild();
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert!(String::from_utf8_lossy(&first.output_files[0].contents).contains("first"));

        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './b.js'; console.log(value)",
        )
        .expect("edit entry");
        std::fs::write(directory.join("b.js"), "export const value = 'second'")
            .expect("create dependency after first build");
        let second = build_context.rebuild();
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        let output = String::from_utf8_lossy(&second.output_files[0].contents);
        assert!(output.contains("second"), "{output}");
        assert!(!output.contains("first"), "{output}");
        std::fs::remove_dir_all(directory).expect("remove context test directory");
    }

    #[test]
    fn build_context_disposal_is_shared_idempotent_and_independent() {
        let directory_a = context_test_directory("dispose-a");
        let directory_b = context_test_directory("dispose-b");
        std::fs::write(directory_a.join("entry.js"), "console.log('a')").expect("write entry a");
        std::fs::write(directory_b.join("entry.js"), "console.log('b')").expect("write entry b");
        let context_a = context(context_options(&directory_a)).expect("create context a");
        let context_a_clone = context_a.clone();
        let context_b = context(context_options(&directory_b)).expect("create context b");

        context_a.dispose();
        context_a.dispose();
        for disposed in [&context_a, &context_a_clone] {
            let result = disposed.rebuild();
            assert!(result.errors.is_empty());
            assert!(result.warnings.is_empty());
            assert!(result.metafile.is_empty());
            assert!(result.output_files.is_empty());
        }

        let independent = context_b.rebuild();
        assert!(independent.errors.is_empty(), "{:?}", independent.errors);
        assert!(
            String::from_utf8_lossy(&independent.output_files[0].contents).contains("console.log")
        );
        std::fs::remove_dir_all(directory_a).expect("remove context test directory a");
        std::fs::remove_dir_all(directory_b).expect("remove context test directory b");
    }

    #[test]
    fn build_context_is_send_sync_and_merges_overlapping_rebuilds() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::BuildContext>();
        assert_send_sync::<super::BuildResult>();
        assert_send_sync::<crate::internal::cache::CacheSet>();
        assert_send_sync::<crate::internal::config::Options>();

        let build_context = context(BuildOptions::default()).expect("create empty context");
        let invocation_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let first_context = build_context.clone();
        let first_count = invocation_count.clone();
        let first = std::thread::spawn(move || {
            first_context.rebuild_with(
                move || {
                    first_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    started_sender.send(()).expect("signal active rebuild");
                    release_receiver.recv().expect("release active rebuild");
                    super::BuildResult {
                        metafile: "merged".into(),
                        ..super::BuildResult::default()
                    }
                },
                || {},
            )
        });
        started_receiver.recv().expect("first rebuild started");

        let (joined_sender, joined_receiver) = std::sync::mpsc::channel();
        let second_context = build_context.clone();
        let second_count = invocation_count.clone();
        let second = std::thread::spawn(move || {
            second_context.rebuild_with(
                move || {
                    second_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    super::BuildResult {
                        metafile: "unexpected second build".into(),
                        ..super::BuildResult::default()
                    }
                },
                move || joined_sender.send(()).expect("signal merged rebuild"),
            )
        });
        joined_receiver.recv().expect("second rebuild merged");
        release_sender.send(()).expect("finish active rebuild");

        let first_result = first.join().expect("first rebuild thread");
        let second_result = second.join().expect("second rebuild thread");
        assert_eq!(first_result.metafile, "merged");
        assert_eq!(second_result.metafile, "merged");
        assert_eq!(
            invocation_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn build_context_dispose_waits_for_an_active_rebuild() {
        let build_context = context(BuildOptions::default()).expect("create empty context");
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let rebuild_context = build_context.clone();
        let rebuild = std::thread::spawn(move || {
            rebuild_context.rebuild_with(
                move || {
                    started_sender.send(()).expect("signal active rebuild");
                    release_receiver.recv().expect("release active rebuild");
                    super::BuildResult::default()
                },
                || {},
            )
        });
        started_receiver.recv().expect("rebuild started");

        let (waiting_sender, waiting_receiver) = std::sync::mpsc::channel();
        let dispose_context = build_context.clone();
        let dispose = std::thread::spawn(move || {
            dispose_context
                .dispose_with(move || waiting_sender.send(()).expect("signal dispose wait"));
        });
        waiting_receiver
            .recv()
            .expect("dispose observed the active rebuild");
        assert!(!dispose.is_finished());
        release_sender.send(()).expect("finish active rebuild");
        rebuild.join().expect("rebuild thread");
        dispose.join().expect("dispose thread");
        assert!(build_context.rebuild().output_files.is_empty());
    }

    #[test]
    fn build_context_tracks_written_outputs_across_rebuilds() {
        let directory = context_test_directory("written-outputs");
        std::fs::write(
            directory.join("entry.js"),
            "import('./lazy.js').then(console.log)",
        )
        .expect("write splitting entry");
        std::fs::write(directory.join("lazy.js"), "export default 'lazy'")
            .expect("write splitting dependency");
        let build_context = context(BuildOptions {
            splitting: true,
            format: BuildFormat::EsModule,
            write: true,
            ..context_options(&directory)
        })
        .expect("create writing context");

        let first = build_context.rebuild();
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert!(first.output_files.len() >= 2, "{:?}", first.output_files);
        let entry_output = first
            .output_files
            .iter()
            .find(|output| {
                std::path::Path::new(&output.path)
                    .file_name()
                    .is_some_and(|name| name == "entry.js")
            })
            .expect("entry output");
        let entry_path = std::path::PathBuf::from(&entry_output.path);
        let expected_entry_contents = entry_output.contents.clone();
        let first_modified = std::fs::metadata(&entry_path)
            .and_then(|metadata| metadata.modified())
            .expect("entry output modification time");

        std::thread::sleep(std::time::Duration::from_millis(30));
        let unchanged = build_context.rebuild();
        assert!(unchanged.errors.is_empty(), "{:?}", unchanged.errors);
        let unchanged_modified = std::fs::metadata(&entry_path)
            .and_then(|metadata| metadata.modified())
            .expect("unchanged output modification time");
        assert_eq!(first_modified, unchanged_modified);

        std::fs::write(&entry_path, "externally modified").expect("tamper with output");
        let repaired = build_context.rebuild();
        assert!(repaired.errors.is_empty(), "{:?}", repaired.errors);
        assert_eq!(
            std::fs::read(&entry_path).expect("read repaired output"),
            expected_entry_contents
        );

        std::fs::write(directory.join("entry.js"), "console.log('no chunk')")
            .expect("remove dynamic import");
        let reduced = build_context.rebuild();
        assert!(reduced.errors.is_empty(), "{:?}", reduced.errors);
        let reduced_paths = reduced
            .output_files
            .iter()
            .map(|output| output.path.as_str())
            .collect::<std::collections::HashSet<_>>();
        for old_output in &first.output_files {
            if !reduced_paths.contains(old_output.path.as_str()) {
                assert!(
                    !std::path::Path::new(&old_output.path).exists(),
                    "stale output was not removed: {}",
                    old_output.path
                );
            }
        }

        std::fs::write(directory.join("entry.js"), "if (").expect("introduce syntax error");
        let failed = build_context.rebuild();
        assert!(!failed.errors.is_empty());
        assert!(
            !entry_path.exists(),
            "failed rebuild left stale output on disk"
        );

        std::fs::write(directory.join("entry.js"), "console.log('repaired')")
            .expect("repair syntax error");
        let recovered = build_context.rebuild();
        assert!(recovered.errors.is_empty(), "{:?}", recovered.errors);
        assert!(
            entry_path.exists(),
            "recovered rebuild did not restore output"
        );
        std::fs::remove_dir_all(directory).expect("remove context test directory");
    }

    #[test]
    fn build_context_watch_rebuilds_and_recovers_automatically() {
        let directory = context_test_directory("watch-recovery");
        let output_path = directory.join("out.js");
        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './dep.js'; console.log(value)",
        )
        .expect("write watched entry");
        std::fs::write(directory.join("dep.js"), "export const value = 'one'")
            .expect("write watched dependency");
        let build_context = context(BuildOptions {
            bundle: true,
            entry_points: vec!["entry.js".into()],
            outfile: "out.js".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            write: true,
            ..BuildOptions::default()
        })
        .expect("create watched context");

        build_context
            .watch(WatchOptions::default())
            .expect("enable watch mode");
        assert_eq!(
            build_context
                .watch(WatchOptions::default())
                .expect_err("watch mode cannot be enabled twice")
                .message,
            "Watch mode has already been enabled"
        );
        wait_for_context_change("initial watch build", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("one"))
        });

        std::fs::write(directory.join("dep.js"), "export const value = 'two'")
            .expect("edit watched dependency");
        wait_for_context_change("dependency rebuild", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("two"))
        });

        std::fs::write(directory.join("entry.js"), "if (").expect("break watched entry");
        wait_for_context_change("failed rebuild cleanup", || !output_path.exists());

        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './dep.js'; console.log('fixed', value)",
        )
        .expect("repair watched entry");
        wait_for_context_change("recovered watch rebuild", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("fixed"))
        });

        build_context.dispose();
        let stopped_output = std::fs::read(&output_path).expect("read output before stopped edit");
        std::fs::write(
            directory.join("dep.js"),
            "export const value = 'after dispose'",
        )
        .expect("edit dependency after dispose");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            std::fs::read(&output_path).expect("read output after stopped edit"),
            stopped_output
        );
        assert_eq!(
            build_context
                .watch(WatchOptions::default())
                .expect_err("disposed context cannot be watched")
                .message,
            "Cannot watch a disposed context"
        );
        std::fs::remove_dir_all(directory).expect("remove context test directory");
    }

    #[test]
    fn build_context_watch_honors_delay_and_tracks_failed_or_manual_builds() {
        let directory = context_test_directory("watch-delay");
        let output_path = directory.join("out.js");
        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './dep.js'; console.log(value)",
        )
        .expect("write watched entry");
        std::fs::write(directory.join("dep.js"), "export const value = 'initial'")
            .expect("write watched dependency");
        let build_context = context(BuildOptions {
            bundle: true,
            entry_points: vec!["entry.js".into()],
            outfile: "out.js".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            write: true,
            ..BuildOptions::default()
        })
        .expect("create watched context");
        build_context
            .watch(WatchOptions { delay: 250 })
            .expect("enable delayed watch mode");
        wait_for_context_change("initial delayed watch build", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("initial"))
        });

        std::fs::write(directory.join("dep.js"), "export const value = 'delayed'")
            .expect("edit delayed dependency");
        std::thread::sleep(std::time::Duration::from_millis(125));
        assert!(
            std::fs::read_to_string(&output_path)
                .expect("read output during delay")
                .contains("initial")
        );
        wait_for_context_change("delayed dependency rebuild", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("delayed"))
        });

        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './missing.js'; console.log(value)",
        )
        .expect("introduce missing watched dependency");
        wait_for_context_change("missing dependency failure", || !output_path.exists());
        std::fs::write(
            directory.join("missing.js"),
            "export const value = 'appeared'",
        )
        .expect("create missing watched dependency");
        wait_for_context_change("missing dependency recovery", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("appeared"))
        });

        std::fs::write(
            directory.join("manual.js"),
            "export const value = 'manual one'",
        )
        .expect("write manual dependency");
        std::fs::write(
            directory.join("entry.js"),
            "import {value} from './manual.js'; console.log(value)",
        )
        .expect("switch dependency before manual build");
        let manual = build_context.rebuild();
        assert!(manual.errors.is_empty(), "{:?}", manual.errors);
        std::fs::write(
            directory.join("manual.js"),
            "export const value = 'manual two'",
        )
        .expect("edit dependency from manual build");
        wait_for_context_change("manual build watch snapshot", || {
            std::fs::read_to_string(&output_path).is_ok_and(|output| output.contains("manual two"))
        });

        build_context.dispose();
        std::fs::remove_dir_all(directory).expect("remove context test directory");
    }

    #[test]
    fn build_context_watch_waits_for_a_preexisting_non_watch_build() {
        let directory = context_test_directory("watch-active-build");
        let output_path = directory.join("out.js");
        std::fs::write(directory.join("entry.js"), "console.log('watch build')")
            .expect("write watched entry");
        let build_context = context(BuildOptions {
            bundle: true,
            entry_points: vec!["entry.js".into()],
            outfile: "out.js".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            write: true,
            ..BuildOptions::default()
        })
        .expect("create watched context");

        let preexisting = std::sync::Arc::new(super::InFlightBuild::default());
        build_context
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active = Some(preexisting.clone());
        build_context
            .watch(WatchOptions::default())
            .expect("enable watch mode during active build");
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !output_path.exists(),
            "watch startup incorrectly reused the non-watch build"
        );

        {
            let mut state = build_context
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                state
                    .active
                    .as_ref()
                    .is_some_and(|active| std::sync::Arc::ptr_eq(active, &preexisting))
            );
            state.active = None;
            preexisting.finish(super::InFlightOutcome::Completed(
                super::BuildResult::default(),
            ));
        }
        wait_for_context_change("distinct initial watch build", || output_path.exists());

        build_context.dispose();
        std::fs::remove_dir_all(directory).expect("remove context test directory");
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
    fn selects_absolute_code_log_and_metafile_paths_independently() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-abs-paths-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        let broken = directory.join("broken.js");
        std::fs::write(&entry, "export const answer = 42").expect("write entry");
        std::fs::write(&broken, "export const = 42").expect("write broken entry");
        let absolute_entry = std::fs::canonicalize(&entry)
            .expect("canonicalize entry")
            .to_string_lossy()
            .into_owned();
        let absolute_broken = std::fs::canonicalize(&broken)
            .expect("canonicalize broken entry")
            .to_string_lossy()
            .into_owned();

        let build_entry = |abs_paths| {
            build(BuildOptions {
                entry_points: vec!["entry.js".into()],
                outdir: "out".into(),
                abs_working_dir: directory.to_string_lossy().into_owned(),
                metafile: true,
                abs_paths,
                ..BuildOptions::default()
            })
        };
        let relative = build_entry(AbsPaths::default());
        let code_only = build_entry(AbsPaths::CODE);
        let metafile_only = build_entry(AbsPaths::METAFILE);
        for result in [&relative, &code_only, &metafile_only] {
            assert!(result.errors.is_empty(), "{:?}", result.errors);
        }

        let relative_code = String::from_utf8_lossy(&relative.output_files[0].contents);
        let absolute_code = String::from_utf8_lossy(&code_only.output_files[0].contents);
        let metafile_code = String::from_utf8_lossy(&metafile_only.output_files[0].contents);
        assert!(relative_code.contains("// entry.js"), "{relative_code}");
        assert!(!relative_code.contains(&absolute_entry), "{relative_code}");
        assert!(
            absolute_code.contains(&format!("// {absolute_entry}")),
            "{absolute_code}"
        );
        assert!(!metafile_code.contains(&absolute_entry), "{metafile_code}");

        let relative_metafile: serde_json::Value =
            serde_json::from_str(&relative.metafile).expect("relative metafile is JSON");
        let absolute_metafile: serde_json::Value =
            serde_json::from_str(&metafile_only.metafile).expect("absolute metafile is JSON");
        assert!(relative_metafile["inputs"].get("entry.js").is_some());
        assert!(
            absolute_metafile["inputs"]
                .get(absolute_entry.as_str())
                .is_some(),
            "{absolute_metafile}"
        );

        let build_broken = |abs_paths| {
            build(BuildOptions {
                entry_points: vec!["broken.js".into()],
                outdir: "out".into(),
                abs_working_dir: directory.to_string_lossy().into_owned(),
                abs_paths,
                ..BuildOptions::default()
            })
        };
        let relative_error = build_broken(AbsPaths::CODE);
        let absolute_error = build_broken(AbsPaths::LOG);
        assert_eq!(
            relative_error.errors[0]
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some("broken.js")
        );
        assert_eq!(
            absolute_error.errors[0]
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some(absolute_broken.as_str())
        );

        let combined = AbsPaths::CODE | AbsPaths::LOG | AbsPaths::METAFILE;
        assert!(combined.contains(AbsPaths::CODE));
        assert!(combined.contains(AbsPaths::LOG));
        assert!(combined.contains(AbsPaths::METAFILE));
        assert!(!AbsPaths::CODE.contains(AbsPaths::LOG));
        assert!(!AbsPaths::CODE.contains(AbsPaths::CODE | AbsPaths::LOG));

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
    fn transforms_module_formats_and_iife_global_names() {
        let common_js = transform(
            "export const x = 1",
            TransformOptions {
                format: BuildFormat::CommonJs,
                ..TransformOptions::default()
            },
        );
        let common_js = code(common_js);
        assert!(
            common_js.contains("module.exports = __toCommonJS"),
            "{common_js}"
        );
        assert!(common_js.contains("const x = 1;"), "{common_js}");
        assert!(
            common_js.len() < 1_500,
            "formatted transforms must tree-shake the helper runtime: {} bytes",
            common_js.len()
        );
        assert!(!common_js.contains("__using"), "{common_js}");

        let es_module = transform(
            "export const x = 1",
            TransformOptions {
                format: BuildFormat::EsModule,
                ..TransformOptions::default()
            },
        );
        let es_module = code(es_module);
        assert!(es_module.contains("const x = 1;"), "{es_module}");
        assert!(es_module.contains("export { x };"), "{es_module}");

        let iife = transform(
            "export const x = 1",
            TransformOptions {
                format: BuildFormat::Iife,
                global_name: "My.Library".into(),
                ..TransformOptions::default()
            },
        );
        let iife = code(iife);
        assert!(iife.starts_with("var My;\n"), "{iife}");
        assert!(iife.contains("(My ||= {}).Library = (() => {"), "{iife}");
        assert!(iife.contains("return __toCommonJS"), "{iife}");

        let invalid = transform(
            "export const x = 1",
            TransformOptions {
                global_name: "not/a/global".into(),
                ..TransformOptions::default()
            },
        );
        assert!(!invalid.errors.is_empty());
        assert!(invalid.code.is_empty());
        assert_eq!(
            invalid.errors[0]
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some("(global name)")
        );

        let baseline = transform("export const x = 1", TransformOptions::default());
        let ignored = transform(
            "export const x = 1",
            TransformOptions {
                global_name: "Valid.ButUnused".into(),
                ..TransformOptions::default()
            },
        );
        assert!(ignored.errors.is_empty(), "{:?}", ignored.errors);
        assert_eq!(ignored.code, baseline.code);
    }

    #[test]
    fn formatted_transforms_preserve_external_source_maps() {
        let result = transform(
            "export const answer = 42",
            TransformOptions {
                sourcefile: "input.js".into(),
                format: BuildFormat::CommonJs,
                sourcemap: BuildSourceMap::External,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(!result.code.is_empty());
        assert!(!result.map.is_empty());
        assert!(!String::from_utf8_lossy(&result.code).contains("sourceMappingURL"));
        let source_map: serde_json::Value =
            serde_json::from_slice(&result.map).expect("formatted transform source map is JSON");
        assert_eq!(source_map["version"], 3);
        assert_eq!(source_map["sources"][0], "input.js");
    }

    #[test]
    fn formatted_css_transforms_route_banner_and_footer() {
        let result = transform(
            "a { color: red }",
            TransformOptions {
                loader: Loader::Css,
                format: BuildFormat::EsModule,
                banner: "/* before */".into(),
                footer: "/* after */".into(),
                ..TransformOptions::default()
            },
        );
        let output = code(result);
        assert!(output.starts_with("/* before */\n"), "{output}");
        assert!(output.ends_with("/* after */\n"), "{output}");
        assert!(output.contains("color: red"), "{output}");
    }

    #[test]
    fn formatted_transforms_reject_invalid_utf8_without_replacement() {
        let result = transform(
            [0xff],
            TransformOptions {
                format: BuildFormat::CommonJs,
                ..TransformOptions::default()
            },
        );
        assert!(result.code.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert_eq!(
            result.errors[0].text,
            "Formatted transform input must be valid UTF-8"
        );
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
    fn validates_target_constraints_and_feature_names() {
        let feature_log = crate::internal::logger::Log::new_defer(
            crate::internal::logger::DeferLogKind::All,
            HashMap::new(),
        );
        let validated = super::validate_target_features(
            &feature_log,
            Target::Es2018,
            &[
                Engine {
                    name: EngineName::Node,
                    version: "8.1".into(),
                },
                Engine {
                    name: EngineName::Chrome,
                    version: "90".into(),
                },
            ],
            &HashMap::new(),
            BuildPlatform::Browser,
        );
        assert_eq!(
            validated.original_target_environment,
            "\"chrome90\", \"es2018\", \"node8.1\""
        );
        assert!(feature_log.done().is_empty());

        let invalid_version = transform(
            "",
            TransformOptions {
                engines: vec![Engine {
                    name: EngineName::Node,
                    version: "1.2.3.4".into(),
                }],
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            invalid_version
                .errors
                .first()
                .map(|message| message.text.as_str()),
            Some("Invalid version: \"1.2.3.4\"")
        );
        let invalid_feature = transform(
            "",
            TransformOptions {
                supported: HashMap::from([("Optional-Catch-Binding".into(), true)]),
                ..TransformOptions::default()
            },
        );
        assert!(
            invalid_feature.errors[0]
                .text
                .contains("not a valid feature name")
        );
    }

    #[test]
    fn applies_target_engines_and_supported_feature_overrides() {
        let source = "try { x() } catch { y() }";
        let transform_for = |target, engines, supported| {
            transform(
                source,
                TransformOptions {
                    target,
                    engines,
                    supported,
                    ..TransformOptions::default()
                },
            )
        };
        assert_eq!(
            code(transform_for(Target::Es2018, Vec::new(), HashMap::new())),
            "try {\n  x();\n} catch (e) {\n  y();\n}\n"
        );
        assert_eq!(
            code(transform_for(Target::Es2019, Vec::new(), HashMap::new())),
            "try {\n  x();\n} catch {\n  y();\n}\n"
        );
        assert_eq!(
            code(transform_for(
                Target::Default,
                vec![Engine {
                    name: EngineName::Node,
                    version: "8".into(),
                }],
                HashMap::new(),
            )),
            "try {\n  x();\n} catch (e) {\n  y();\n}\n"
        );
        assert_eq!(
            code(transform_for(
                Target::Es2018,
                Vec::new(),
                HashMap::from([("optional-catch-binding".into(), true)]),
            )),
            "try {\n  x();\n} catch {\n  y();\n}\n"
        );
        assert_eq!(
            code(transform_for(
                Target::EsNext,
                Vec::new(),
                HashMap::from([("optional-catch-binding".into(), false)]),
            )),
            "try {\n  x();\n} catch (e) {\n  y();\n}\n"
        );

        let built = build_api(BuildOptions {
            bundle: true,
            stdin: Some(BuildStdin {
                contents: source.into(),
                ..BuildStdin::default()
            }),
            target: Target::Es2018,
            ..BuildOptions::default()
        });
        assert!(built.errors.is_empty(), "{:?}", built.errors);
        assert!(
            String::from_utf8_lossy(&built.output_files[0].contents).contains("catch (e)"),
            "{}",
            String::from_utf8_lossy(&built.output_files[0].contents)
        );
    }

    #[test]
    fn lowers_plain_exponentiation_at_target_and_override_boundaries() {
        let source = "let result = a ** b;";
        let lowered = concat!("var __pow = Math.pow;\n", "let result = __pow(a, b);\n");
        let preserved = "let result = a ** b;\n";

        assert_eq!(
            transform_code(
                source,
                TransformOptions {
                    target: Target::Es2015,
                    ..TransformOptions::default()
                },
            ),
            lowered
        );
        assert_eq!(
            transform_code(
                source,
                TransformOptions {
                    target: Target::Es2016,
                    ..TransformOptions::default()
                },
            ),
            preserved
        );
        assert_eq!(
            transform_code(
                source,
                TransformOptions {
                    target: Target::Es2015,
                    supported: HashMap::from([("exponent-operator".into(), true)]),
                    ..TransformOptions::default()
                },
            ),
            preserved
        );
        assert_eq!(
            transform_code(
                source,
                TransformOptions {
                    target: Target::EsNext,
                    supported: HashMap::from([("exponent-operator".into(), false)]),
                    ..TransformOptions::default()
                },
            ),
            lowered
        );
    }

    #[test]
    fn guards_unlowered_exponentiation_assignment_at_target_and_override_boundaries() {
        let source = "base **= power";
        let message = "Transforming exponentiation assignment operators to the configured target \
                       environment (\"es2015\") is not supported yet";
        assert_transform_error(
            &transform(
                source,
                TransformOptions {
                    target: Target::Es2015,
                    ..TransformOptions::default()
                },
            ),
            message,
            5,
            3,
        );

        assert_eq!(
            transform_code(
                source,
                TransformOptions {
                    target: Target::Es2016,
                    ..TransformOptions::default()
                },
            ),
            "base **= power;\n"
        );
        assert_eq!(
            transform_code(
                source,
                TransformOptions {
                    target: Target::Es2015,
                    supported: HashMap::from([("exponent-operator".into(), true)]),
                    ..TransformOptions::default()
                },
            ),
            "base **= power;\n"
        );
        assert_transform_error(
            &transform(
                source,
                TransformOptions {
                    target: Target::EsNext,
                    supported: HashMap::from([("exponent-operator".into(), false)]),
                    ..TransformOptions::default()
                },
            ),
            "Transforming exponentiation assignment operators to the configured target \
             environment (\"esnext\" + 1 override) is not supported yet",
            5,
            3,
        );

        assert_eq!(
            transform_code(
                "base ** power",
                TransformOptions {
                    target: Target::Es2015,
                    ..TransformOptions::default()
                },
            ),
            "var __pow = Math.pow;\n__pow(base, power);\n"
        );
    }

    #[test]
    fn lowers_plain_exponentiation_with_stable_helper_semantics() {
        let source = "let right=a**b**c;\
                      let left=(a**b)**c;\
                      let operands=(x(),y())**(z(),w());\
                      let updates=x++**--y;";
        assert_eq!(
            transform_code(
                source,
                TransformOptions {
                    target: Target::Es2015,
                    ..TransformOptions::default()
                },
            ),
            concat!(
                "var __pow = Math.pow;\n",
                "let right = __pow(a, __pow(b, c));\n",
                "let left = __pow(__pow(a, b), c);\n",
                "let operands = __pow((x(), y()), (z(), w()));\n",
                "let updates = __pow(x++, --y);\n",
            )
        );

        let collision = transform_code(
            "let __pow = 1; use(__pow, a ** b, c ** d);",
            TransformOptions {
                target: Target::Es2015,
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            collision,
            concat!(
                "var __pow2 = Math.pow;\n",
                "let __pow = 1;\n",
                "use(__pow, __pow2(a, b), __pow2(c, d));\n",
            )
        );
        assert_eq!(collision.matches("Math.pow").count(), 1);
        assert_eq!(collision.matches("__pow2(").count(), 2);

        let with_keep_names = transform_code(
            "function foo() {} foo ** bar",
            TransformOptions {
                target: Target::Es2015,
                keep_names: true,
                ..TransformOptions::default()
            },
        );
        let def_prop = with_keep_names.find("var __defProp").expect("__defProp");
        let pow = with_keep_names.find("var __pow").expect("__pow");
        let name = with_keep_names.find("var __name").expect("__name");
        let function = with_keep_names.find("function foo").expect("user code");
        assert!(
            def_prop < pow && pow < name && name < function,
            "{with_keep_names}"
        );

        let minify_source = "let result=a**b**c;";
        assert_eq!(
            transform_code(
                minify_source,
                TransformOptions {
                    target: Target::Es2015,
                    minify_syntax: true,
                    ..TransformOptions::default()
                },
            ),
            "var __pow = Math.pow;\nlet result = __pow(a, __pow(b, c));\n"
        );
        assert_eq!(
            transform_code(
                minify_source,
                TransformOptions {
                    target: Target::Es2015,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                },
            ),
            "var __pow=Math.pow;let result=__pow(a,__pow(b,c));\n"
        );
        assert_eq!(
            transform_code(
                minify_source,
                TransformOptions {
                    target: Target::Es2015,
                    minify_identifiers: true,
                    ..TransformOptions::default()
                },
            ),
            "var e = Math.pow;\nlet result = e(a, e(b, c));\n"
        );
        assert_eq!(
            transform_code(
                minify_source,
                TransformOptions {
                    target: Target::Es2015,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                },
            ),
            "var e=Math.pow;let result=e(a,e(b,c));\n"
        );
    }

    #[test]
    fn bundles_plain_exponentiation_through_the_runtime_helper() {
        let source = "let __pow=1;console.log(__pow,a**b,c**d)";
        let build_for = |target, supported| {
            let result = build_api(BuildOptions {
                bundle: true,
                stdin: Some(BuildStdin {
                    contents: source.into(),
                    ..BuildStdin::default()
                }),
                target,
                supported,
                minify_whitespace: true,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            String::from_utf8(result.output_files[0].contents.clone()).expect("JavaScript output")
        };

        let lowered = build_for(Target::Es2015, HashMap::new());
        assert_eq!(lowered.matches("Math.pow").count(), 1, "{lowered}");
        assert!(lowered.contains("__pow(a,b)"), "{lowered}");
        assert!(lowered.contains("__pow(c,d)"), "{lowered}");

        let supported_boundary = build_for(Target::Es2016, HashMap::new());
        assert!(
            !supported_boundary.contains("Math.pow"),
            "{supported_boundary}"
        );
        assert!(supported_boundary.contains("a**b"), "{supported_boundary}");

        let forced_supported = build_for(
            Target::Es2015,
            HashMap::from([("exponent-operator".into(), true)]),
        );
        assert!(!forced_supported.contains("Math.pow"), "{forced_supported}");
        assert!(forced_supported.contains("a**b"), "{forced_supported}");

        let forced_unsupported = build_for(
            Target::EsNext,
            HashMap::from([("exponent-operator".into(), false)]),
        );
        assert_eq!(
            forced_unsupported.matches("Math.pow").count(),
            1,
            "{forced_unsupported}"
        );
        assert!(
            forced_unsupported.contains("__pow(a,b)"),
            "{forced_unsupported}"
        );
    }

    #[test]
    fn reports_unlowered_syntax_target_errors_with_exact_ranges() {
        for (source, target, text, column, length) in [
            (
                "class C {}",
                Target::Es5,
                "Transforming class syntax to the configured target environment (\"es5\") is not \
                 supported yet",
                0,
                5,
            ),
            (
                "const x = 1",
                Target::Es5,
                "Transforming const to the configured target environment (\"es5\") is not \
                 supported yet",
                0,
                5,
            ),
            (
                "let x = 1",
                Target::Es5,
                "Transforming let to the configured target environment (\"es5\") is not \
                 supported yet",
                0,
                3,
            ),
            (
                "await work()",
                Target::Es2021,
                "Top-level await is not available in the configured target environment \
                 (\"es2021\")",
                0,
                5,
            ),
        ] {
            assert_transform_error(
                &transform(
                    source,
                    TransformOptions {
                        target,
                        ..TransformOptions::default()
                    },
                ),
                text,
                column,
                length,
            );
        }

        for (source, text, column, length) in [
            (
                "declare class C {}",
                "Transforming class syntax to the configured target environment (\"es5\") is not \
                 supported yet",
                8,
                5,
            ),
            (
                "declare const x: number",
                "Transforming const to the configured target environment (\"es5\") is not \
                 supported yet",
                8,
                5,
            ),
            (
                "declare let x: number",
                "Transforming let to the configured target environment (\"es5\") is not supported \
                 yet",
                8,
                3,
            ),
        ] {
            assert_transform_error(
                &transform(
                    source,
                    TransformOptions {
                        loader: Loader::Ts,
                        target: Target::Es5,
                        ..TransformOptions::default()
                    },
                ),
                text,
                column,
                length,
            );
        }
    }

    #[test]
    fn reports_binding_and_spread_target_errors_with_exact_ranges() {
        let source = "function f({a} = x, ...rest) {}\n\
                      call(...x); new C(...y);\n\
                      [a, {b}] = value;";
        let result = transform(
            source,
            TransformOptions {
                target: Target::Es5,
                ..TransformOptions::default()
            },
        );
        assert!(result.code.is_empty());
        assert!(result.warnings.is_empty());
        let expected = [
            ("destructuring", 1, 11, 1),
            ("default arguments", 1, 15, 1),
            ("rest arguments", 1, 20, 3),
            ("rest arguments", 2, 5, 3),
            ("rest arguments", 2, 18, 3),
            ("destructuring", 3, 0, 1),
            ("destructuring", 3, 4, 1),
        ];
        assert_eq!(result.errors.len(), expected.len());
        for (message, (name, line, column, length)) in result.errors.iter().zip(expected) {
            assert_api_error(
                message,
                &format!(
                    "Transforming {name} to the configured target environment (\"es5\") is not \
                     supported yet"
                ),
                line,
                column,
                length,
            );
        }

        let built = build_api(BuildOptions {
            stdin: Some(BuildStdin {
                contents: "function f(a = 1, ...rest) {}".into(),
                ..BuildStdin::default()
            }),
            target: Target::Es5,
            ..BuildOptions::default()
        });
        assert!(built.output_files.is_empty());
        assert_eq!(built.errors.len(), 2);
        assert_api_error(
            &built.errors[0],
            "Transforming default arguments to the configured target environment (\"es5\") is \
             not supported yet",
            1,
            13,
            1,
        );
        assert_api_error(
            &built.errors[1],
            "Transforming rest arguments to the configured target environment (\"es5\") is not \
             supported yet",
            1,
            18,
            3,
        );
    }

    #[test]
    fn respects_binding_guard_boundaries_and_supported_overrides() {
        let supported_in_es2015 =
            "function f(a = 1, ...rest) {}; var [value] = input; call(...items)";
        let result = transform(
            supported_in_es2015,
            TransformOptions {
                target: Target::Es2015,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        assert_transform_error(
            &transform(
                "var [...[value]] = input",
                TransformOptions {
                    target: Target::Es2015,
                    ..TransformOptions::default()
                },
            ),
            "Transforming non-identifier array rest patterns to the configured target environment \
             (\"es2015\") is not supported yet",
            8,
            1,
        );
        let result = transform(
            "var [...[value]] = input",
            TransformOptions {
                target: Target::Es2016,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        let result = transform(
            "function f({a} = x, ...rest) {}; call(...items); var [...[value]] = input",
            TransformOptions {
                target: Target::Es5,
                supported: HashMap::from([
                    ("default-argument".into(), true),
                    ("rest-argument".into(), true),
                    ("destructuring".into(), true),
                    ("nested-rest-binding".into(), true),
                ]),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        for (source, feature, name, column, length) in [
            (
                "function f(value = 1) {}",
                "default-argument",
                "default arguments",
                17,
                1,
            ),
            ("call(...items)", "rest-argument", "rest arguments", 5, 3),
            (
                "var [value] = input",
                "destructuring",
                "destructuring",
                4,
                1,
            ),
            (
                "var [...[value]] = input",
                "nested-rest-binding",
                "non-identifier array rest patterns",
                8,
                1,
            ),
        ] {
            assert_transform_error(
                &transform(
                    source,
                    TransformOptions {
                        target: Target::EsNext,
                        supported: HashMap::from([(feature.into(), false)]),
                        ..TransformOptions::default()
                    },
                ),
                &format!(
                    "Transforming {name} to the configured target environment (\"esnext\" + 1 \
                     override) is not supported yet"
                ),
                column,
                length,
            );
        }
    }

    #[test]
    fn reports_generator_family_target_errors_with_exact_ranges() {
        for (source, text, column, length) in [
            (
                "function* f() {}",
                "Transforming generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                8,
                1,
            ),
            (
                "(function* () {})",
                "Transforming generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                9,
                1,
            ),
            (
                "var f = async value => value",
                "Transforming async functions to the configured target environment (\"es5\") is \
                 not supported yet",
                8,
                5,
            ),
            (
                "({ async method() {} })",
                "Transforming async functions to the configured target environment (\"es5\") is \
                 not supported yet",
                3,
                5,
            ),
            (
                "async function* f() {}",
                "Transforming async generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                0,
                5,
            ),
        ] {
            assert_transform_error(
                &transform(
                    source,
                    TransformOptions {
                        target: Target::Es5,
                        ..TransformOptions::default()
                    },
                ),
                text,
                column,
                length,
            );
        }

        for (source, text, column, length) in [
            (
                "declare function* f(): void;",
                "Transforming generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                16,
                1,
            ),
            (
                "export declare async function f(): void;",
                "Transforming async functions to the configured target environment (\"es5\") is \
                 not supported yet",
                15,
                5,
            ),
            (
                "declare async function* f(): void;",
                "Transforming async generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                8,
                5,
            ),
        ] {
            assert_transform_error(
                &transform(
                    source,
                    TransformOptions {
                        loader: Loader::Ts,
                        target: Target::Es5,
                        ..TransformOptions::default()
                    },
                ),
                text,
                column,
                length,
            );
        }
    }

    #[test]
    fn respects_generator_family_boundaries_and_overrides() {
        let all_forms = "function* g() {};\n\
                         async function f() {};\n\
                         async function* h() {};\n\
                         var arrow = async value => value;\n\
                         ({ *g() {}, async f() {}, async *h() {} });\n\
                         class C { *g() {} async f() {} async *h() {} }";
        let result = transform(
            all_forms,
            TransformOptions {
                target: Target::Es2015,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        let result = transform(
            all_forms,
            TransformOptions {
                target: Target::Es5,
                supported: HashMap::from([("generator".into(), true), ("class".into(), true)]),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        assert_transform_error(
            &transform(
                "function* f() {}",
                TransformOptions {
                    target: Target::EsNext,
                    supported: HashMap::from([("generator".into(), false)]),
                    ..TransformOptions::default()
                },
            ),
            "Transforming generator functions to the configured target environment (\"esnext\" \
             + 2 overrides) is not supported yet",
            8,
            1,
        );

        let result = transform(
            "async function f() {}",
            TransformOptions {
                target: Target::EsNext,
                supported: HashMap::from([("generator".into(), false)]),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        assert_transform_error(
            &transform(
                "async function* f() {}",
                TransformOptions {
                    target: Target::EsNext,
                    supported: HashMap::from([
                        ("generator".into(), false),
                        ("async-generator".into(), false),
                    ]),
                    ..TransformOptions::default()
                },
            ),
            "Transforming async generator functions to the configured target environment \
             (\"esnext\" + 2 overrides) is not supported yet",
            0,
            5,
        );
    }

    #[test]
    fn respects_syntax_guard_boundaries_and_supported_overrides() {
        for (source, target) in [
            ("class C {}", Target::Es2015),
            ("const x = 1; let y = 2", Target::Es2015),
            ("await work()", Target::Es2022),
        ] {
            let result = transform(
                source,
                TransformOptions {
                    target,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
        }

        let result = transform(
            "class C {}; const x = 1; let y = 2",
            TransformOptions {
                target: Target::Es5,
                supported: HashMap::from([("class".into(), true), ("const-and-let".into(), true)]),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let result = transform(
            "await work()",
            TransformOptions {
                target: Target::Es2021,
                supported: HashMap::from([("top-level-await".into(), true)]),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        assert_transform_error(
            &transform(
                "class C {}",
                TransformOptions {
                    target: Target::EsNext,
                    supported: HashMap::from([("class".into(), false)]),
                    ..TransformOptions::default()
                },
            ),
            "Transforming class syntax to the configured target environment (\"esnext\" + 11 \
             overrides) is not supported yet",
            0,
            5,
        );
        assert_transform_error(
            &transform(
                "const x = 1",
                TransformOptions {
                    target: Target::EsNext,
                    supported: HashMap::from([("const-and-let".into(), false)]),
                    ..TransformOptions::default()
                },
            ),
            "Transforming const to the configured target environment (\"esnext\" + 1 override) is \
             not supported yet",
            0,
            5,
        );
        assert_transform_error(
            &transform(
                "await work()",
                TransformOptions {
                    target: Target::EsNext,
                    supported: HashMap::from([("top-level-await".into(), false)]),
                    ..TransformOptions::default()
                },
            ),
            "Top-level await is not available in the configured target environment (\"esnext\" + \
             1 override)",
            0,
            5,
        );
    }

    #[test]
    fn rejects_top_level_await_for_cjs_and_iife_formats() {
        for (format, name) in [(BuildFormat::CommonJs, "cjs"), (BuildFormat::Iife, "iife")] {
            assert_transform_error(
                &transform(
                    "await work()",
                    TransformOptions {
                        format,
                        target: Target::EsNext,
                        ..TransformOptions::default()
                    },
                ),
                &format!(
                    "Top-level await is currently not supported with the {name:?} output format"
                ),
                0,
                5,
            );
        }
        let result = transform(
            "await work()",
            TransformOptions {
                format: BuildFormat::EsModule,
                target: Target::EsNext,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn reports_syntax_guard_errors_through_build_api() {
        let built = build_api(BuildOptions {
            stdin: Some(BuildStdin {
                contents: "class C {}".into(),
                ..BuildStdin::default()
            }),
            target: Target::Es5,
            ..BuildOptions::default()
        });
        assert!(built.output_files.is_empty());
        assert!(built.warnings.is_empty());
        assert_eq!(built.errors.len(), 1, "{:?}", built.errors);
        assert_api_error(
            &built.errors[0],
            "Transforming class syntax to the configured target environment (\"es5\") is not \
             supported yet",
            1,
            0,
            5,
        );

        let built = build_api(BuildOptions {
            stdin: Some(BuildStdin {
                contents: "await work()".into(),
                ..BuildStdin::default()
            }),
            format: BuildFormat::CommonJs,
            target: Target::EsNext,
            ..BuildOptions::default()
        });
        assert!(built.output_files.is_empty());
        assert!(built.warnings.is_empty());
        assert_eq!(built.errors.len(), 1, "{:?}", built.errors);
        assert_api_error(
            &built.errors[0],
            "Top-level await is currently not supported with the \"cjs\" output format",
            1,
            0,
            5,
        );
    }

    #[test]
    fn reports_generator_family_errors_through_build_api() {
        let built = build_api(BuildOptions {
            stdin: Some(BuildStdin {
                contents:
                    "function* first() {}\nasync function second() {}\nasync function* third() {}"
                        .into(),
                ..BuildStdin::default()
            }),
            target: Target::Es5,
            ..BuildOptions::default()
        });
        assert!(built.output_files.is_empty());
        assert!(built.warnings.is_empty());
        assert_eq!(built.errors.len(), 3, "{:?}", built.errors);
        for (message, (text, line, column, length)) in built.errors.iter().zip([
            (
                "Transforming generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                1,
                8,
                1,
            ),
            (
                "Transforming async functions to the configured target environment (\"es5\") is \
                 not supported yet",
                2,
                0,
                5,
            ),
            (
                "Transforming async generator functions to the configured target environment \
                 (\"es5\") is not supported yet",
                3,
                0,
                5,
            ),
        ]) {
            assert_api_error(message, text, line, column, length);
        }
    }

    #[test]
    fn lowers_bigint_literals_for_unsupported_targets() {
        assert_eq!(
            transform_code(
                "x = 0b100101n",
                TransformOptions {
                    target: Target::Es2020,
                    ..TransformOptions::default()
                }
            ),
            "x = 0b100101n;\n"
        );
        assert_eq!(
            transform_code(
                "x = 0b100101n",
                TransformOptions {
                    target: Target::Es2019,
                    ..TransformOptions::default()
                }
            ),
            "x = /* @__PURE__ */ BigInt(\"0b100101\");\n"
        );
        let warned = transform(
            "x = 1n",
            TransformOptions {
                target: Target::Es2019,
                ..TransformOptions::default()
            },
        );
        assert_eq!(warned.warnings.len(), 1);
        assert_eq!(warned.warnings[0].id, "bigint");
        assert_eq!(
            warned.warnings[0].text,
            "Big integer literals are not available in the configured target environment \
             (\"es2019\") and may crash at run-time"
        );
        let location = warned.warnings[0]
            .location
            .as_ref()
            .expect("BigInt warning location");
        assert_eq!(location.line, 1);
        assert_eq!(location.column, 4);
        assert_eq!(location.length, 2);
        assert_eq!(location.line_text, "x = 1n");
        assert_eq!(
            transform_code(
                "x = 1n",
                TransformOptions {
                    target: Target::Es2019,
                    supported: HashMap::from([("bigint".into(), true)]),
                    ..TransformOptions::default()
                }
            ),
            "x = 1n;\n"
        );
        assert_eq!(
            transform_code(
                "x = 1n",
                TransformOptions {
                    target: Target::Es2020,
                    supported: HashMap::from([("bigint".into(), false)]),
                    ..TransformOptions::default()
                }
            ),
            "x = /* @__PURE__ */ BigInt(\"1\");\n"
        );
    }

    #[test]
    fn minifies_lowered_bigint_literals() {
        assert_eq!(
            transform_code(
                "x = 0b100101n",
                TransformOptions {
                    target: Target::Es2019,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            ),
            "x = /* @__PURE__ */ BigInt(37);\n"
        );
        assert_eq!(
            transform_code(
                "x = 0b100101n",
                TransformOptions {
                    target: Target::Es2019,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            ),
            "x=BigInt(\"0b100101\");\n"
        );
        assert_eq!(
            transform_code(
                "x = 0b100101n",
                TransformOptions {
                    target: Target::Es2019,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            ),
            "x=BigInt(37);\n"
        );
        assert_eq!(
            transform_code(
                "x=0XFFn;y=0B101n;z=0O77n",
                TransformOptions {
                    target: Target::Es2019,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            ),
            "x = /* @__PURE__ */ BigInt(255), y = /* @__PURE__ */ BigInt(5), z = /* @__PURE__ */ BigInt(63);\n"
        );

        assert_eq!(
            transform_code(
                "x = -123n; y = 0xFEDCBA9876543210n",
                TransformOptions {
                    target: Target::Es2019,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            ),
            "x = -/* @__PURE__ */ BigInt(123), y = /* @__PURE__ */ \
             BigInt(\"0xFEDCBA9876543210\");\n"
        );
        assert_eq!(
            transform_code(
                "1n; -2n; keep()",
                TransformOptions {
                    target: Target::Es2019,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            ),
            "keep();\n"
        );
    }

    #[test]
    fn lowers_bigint_literals_in_expression_contexts() {
        assert_eq!(
            transform_code(
                "x = 1n.toString(); y = new (1n)()",
                TransformOptions {
                    target: Target::Es2019,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            ),
            "x=BigInt(\"1\").toString();y=new(BigInt(\"1\"));\n"
        );
        assert_eq!(
            transform_code(
                "x = {1n: y, [2n]: z}",
                TransformOptions {
                    target: Target::Es2019,
                    ..TransformOptions::default()
                }
            ),
            "x = { \"1\": y, [/* @__PURE__ */ BigInt(\"2\")]: z };\n"
        );
        let property_keys = transform(
            "x = {1n: y, [2n]: z}",
            TransformOptions {
                target: Target::Es2019,
                ..TransformOptions::default()
            },
        );
        assert_eq!(property_keys.warnings.len(), 1);
        assert_eq!(
            transform_code(
                "function f(BigInt) { return 1n }",
                TransformOptions {
                    target: Target::Es2019,
                    ..TransformOptions::default()
                }
            ),
            "function f(BigInt2) {\n  return /* @__PURE__ */ BigInt(\"1\");\n}\n"
        );
    }

    #[test]
    fn uses_contextual_bigint_warning_visibility() {
        let contextual = transform(
            "try {\n\
             \x20 inside = 1n;\n\
             \x20 function nested() { return 2n }\n\
             } catch {\n\
             \x20 caught = 3n\n\
             } finally {\n\
             \x20 final = 4n\n\
             }",
            TransformOptions {
                target: Target::Es2019,
                ..TransformOptions::default()
            },
        );
        assert!(contextual.errors.is_empty(), "{:?}", contextual.errors);
        assert_eq!(
            String::from_utf8_lossy(&contextual.code)
                .matches("BigInt(")
                .count(),
            4
        );
        assert_eq!(contextual.warnings.len(), 3);
        assert!(
            contextual
                .warnings
                .iter()
                .all(|warning| warning.id == "bigint")
        );
        assert_eq!(
            contextual
                .warnings
                .iter()
                .map(|warning| {
                    warning
                        .location
                        .as_ref()
                        .expect("BigInt warning location")
                        .line
                })
                .collect::<Vec<_>>(),
            [3, 5, 7]
        );

        let dependency = transform(
            "value = 0XFFn",
            TransformOptions {
                sourcefile: "/project/node_modules/pkg/index.js".into(),
                target: Target::Es2019,
                ..TransformOptions::default()
            },
        );
        assert!(dependency.errors.is_empty(), "{:?}", dependency.errors);
        assert!(dependency.warnings.is_empty(), "{:?}", dependency.warnings);
        assert_eq!(
            String::from_utf8_lossy(&dependency.code),
            "value = /* @__PURE__ */ BigInt(\"0XFF\");\n"
        );
    }

    #[test]
    fn lowers_bigint_literals_in_build_api() {
        let built = build_api(BuildOptions {
            stdin: Some(BuildStdin {
                contents: "x = 0b100101n".into(),
                ..BuildStdin::default()
            }),
            target: Target::Es2019,
            ..BuildOptions::default()
        });
        assert!(built.errors.is_empty(), "{:?}", built.errors);
        assert_eq!(built.warnings.len(), 1);
        assert_eq!(built.warnings[0].id, "bigint");
        assert_eq!(
            String::from_utf8_lossy(&built.output_files[0].contents),
            "x = /* @__PURE__ */ BigInt(\"0b100101\");\n"
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
    fn exposes_transform_tree_shaking_control() {
        let input = "const dead = 1; console.log('live')";
        let default = code(transform(input, TransformOptions::default()));
        let enabled = code(transform(
            input,
            TransformOptions {
                tree_shaking: BuildTreeShaking::Enabled,
                ..TransformOptions::default()
            },
        ));
        assert!(default.contains("dead"), "{default}");
        assert!(!enabled.contains("dead"), "{enabled}");

        let iife_default = code(transform(
            input,
            TransformOptions {
                format: BuildFormat::Iife,
                ..TransformOptions::default()
            },
        ));
        let iife_disabled = code(transform(
            input,
            TransformOptions {
                format: BuildFormat::Iife,
                tree_shaking: BuildTreeShaking::Disabled,
                ..TransformOptions::default()
            },
        ));
        assert!(!iife_default.contains("dead"), "{iife_default}");
        assert!(iife_disabled.contains("dead"), "{iife_disabled}");
        assert!(enabled.contains("console.log(\"live\")"), "{enabled}");
    }

    #[test]
    fn transform_tree_shaking_preserves_binary_loader_inputs() {
        for loader in [Loader::Text, Loader::Binary] {
            let baseline = transform(
                [0xff],
                TransformOptions {
                    loader,
                    ..TransformOptions::default()
                },
            );
            assert!(baseline.errors.is_empty(), "{:?}", baseline.errors);
            for tree_shaking in [BuildTreeShaking::Enabled, BuildTreeShaking::Disabled] {
                let result = transform(
                    [0xff],
                    TransformOptions {
                        loader,
                        tree_shaking,
                        ..TransformOptions::default()
                    },
                );
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                assert_eq!(result.code, baseline.code, "{loader:?} {tree_shaking:?}");
            }
        }
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
    fn lowers_unsupported_css_features_in_builds() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("esbuild-rs-css-gradient-lowering-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        std::fs::write(
            directory.join("entry.css"),
            ".entry { color: ReBeCcApUrPlE; border-color: #1234; \
             background: linear-gradient(red 10% 20%, blue) }",
        )
        .expect("write CSS entry");

        let result = build(BuildOptions {
            entry_points: vec!["entry.css".into()],
            outdir: "out".into(),
            abs_working_dir: directory.to_string_lossy().into_owned(),
            supported: HashMap::from([
                ("gradient-double-position".into(), false),
                ("hex-rgba".into(), false),
                ("rebecca-purple".into(), false),
            ]),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(output.contains("red 10%,\n      red 20%"), "{output}");
        assert!(!output.contains("red 10% 20%"), "{output}");
        assert!(output.contains("color: #663399"), "{output}");
        assert!(
            output.contains("border-color: rgba(17, 34, 51, .267)"),
            "{output}"
        );

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
            literal_template.contains("this.message = `hello`;"),
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
    fn minifies_typescript_namespace_aliases_like_esbuild() {
        for (input, expected) in [
            (
                "namespace foo{export let foo=123;console.log(foo)}",
                "var foo;(o=>(o.foo=123,console.log(o.foo)))(foo||={});\n",
            ),
            (
                "namespace N{export namespace N{export const x=1}}",
                "var N;(a=>{let e;(p=>p.x=1)(e=a.N||={})})(N||={});\n",
            ),
            (
                "namespace N{export const x=1;export function f(){return x}console.log(x,f())}",
                "var N;(e=>{e.x=1;function o(){return 1}e.f=o,console.log(1,o())})(N||={});\n",
            ),
            (
                "namespace N{enum E{A};enum F{B}}",
                "var N;(a=>{let m;(e=>e[e.A=0]=\"A\")(m||={});let n;(e=>e[e.B=0]=\"B\")(n||={})})(N||={});\n",
            ),
            (
                "namespace N{enum E{A};enum F{B};console.log(E,F)}",
                "var N;(o=>{let n;(e=>e[e.A=0]=\"A\")(n||={});let m;(e=>e[e.B=0]=\"B\")(m||={}),console.log(n,m)})(N||={});\n",
            ),
            (
                "namespace N{export enum E{A};console.log(E.A)}",
                "var N;(n=>{let o;(e=>e[e.A=0]=\"A\")(o=n.E||={}),console.log(0)})(N||={});\n",
            ),
        ] {
            assert_eq!(
                code(transform(
                    input,
                    TransformOptions {
                        loader: Loader::Ts,
                        minify_syntax: true,
                        minify_identifiers: true,
                        minify_whitespace: true,
                        ..TransformOptions::default()
                    }
                )),
                expected
            );
        }
    }

    #[test]
    fn reuses_typescript_closure_argument_names_in_sibling_scopes() {
        for (input, expected) in [
            (
                "enum E{A};enum E{B};console.log(E)",
                "var E=(E2=>{E2[E2[\"A\"]=0]=\"A\";return E2})(E||{});;\
                 var E=(E2=>{E2[E2[\"B\"]=0]=\"B\";return E2})(E||{});;\
                 console.log(E);\n",
            ),
            (
                "namespace N{let a=1};namespace N{let b=2}",
                "var N;(N2=>{let a=1})(N||(N={}));;(N2=>{let b=2})(N||(N={}));\n",
            ),
        ] {
            assert_eq!(
                code(transform(
                    input,
                    TransformOptions {
                        loader: Loader::Ts,
                        minify_whitespace: true,
                        ..TransformOptions::default()
                    }
                )),
                expected
            );
        }
    }

    #[test]
    fn rejects_optional_chains_used_directly_as_template_tags() {
        for input in [
            "a?.b``",
            "a?.(b)``",
            "a?.[b]``",
            "a?.b.c`${d}`",
            "a?.(b).c`${d}`",
            "a?.[b].c`${d}`",
        ] {
            let result = transform(input, TransformOptions::default());
            assert_eq!(result.errors.len(), 1, "{input}: {:?}", result.errors);
            assert_eq!(
                result.errors[0].text,
                "Template literals cannot have an optional chain as a tag"
            );
        }

        for input in ["(a?.b)``", "(a?.(b))``", "(a?.[b])`${d}`"] {
            let result = transform(input, TransformOptions::default());
            assert!(result.errors.is_empty(), "{input}: {:?}", result.errors);
        }
    }

    #[test]
    fn preserves_keyword_boundaries_and_exponentiation_grammar() {
        assert_eq!(
            code(transform(
                "let a=(-x)**y,b=(!x)**y,c=(typeof x)**y,d=(void 0)**y,e=(-1)**y,f=(++x)**y;\
                 if(a)b();else e();\
                 if(a)b();else [e];\
                 if(a)b();else 0;\
                 async function g(){return (await x)**y}",
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "let a=(-x)**y,b=(!x)**y,c=(typeof x)**y,d=(void 0)**y,e=(-1)**y,f=++x**y;\
             if(a)b();else e();if(a)b();else[e];if(a)b();else 0;\
             async function g(){return(await x)**y}\n"
        );
    }

    #[test]
    fn marks_pure_iifes_and_inlines_simple_arrow_bodies() {
        assert_eq!(
            code(transform(
                "(e=>e)(x);var a=(function(){})()",
                TransformOptions::default()
            )),
            concat!(
                "/* @__PURE__ */ ((e) => e)(x);\n",
                "var a = /* @__PURE__ */ (function() {\n",
                "})();\n",
            )
        );
        assert_eq!(
            code(transform(
                "(e=>e)(x);(()=>a)(...[]);function f(){return (()=>x)()}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!("x, a;\n", "function f() {\n", "  return x;\n", "}\n")
        );
        assert_eq!(
            code(transform(
                "x=(()=>1);y=(function(){})",
                TransformOptions::default()
            )),
            concat!("x = (() => 1);\n", "y = (function() {\n", "});\n")
        );
        assert_eq!(
            code(transform(
                "x=(()=>1);y=(function(){})",
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "x=(()=>1);y=(function(){});\n"
        );
        assert_eq!(
            code(transform("(function*(){})()", TransformOptions::default())),
            concat!("(function* () {\n", "})();\n")
        );
        assert_eq!(
            code(transform(
                "let x=()=>{let y=()=>{z()};y()}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!("let x = () => {\n", "  z();\n", "};\n")
        );
    }

    #[test]
    fn distributes_safe_operators_out_of_comma_left_operands() {
        assert_eq!(
            code(transform(
                "function unary(){return [-(a,b),+(a,b),~(a,b),!(a,b),void(a,b),\
                 typeof(a,b),delete(a,b)]}\
                 (a,b)&&c;(a,b)==c;(a,b)+c;a&&(b,c);a==(b,c);a+(b,c)",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "function unary() {\n",
                "  return [(a, -b), (a, +b), (a, ~b), (a, !b), (a, void b), ",
                "typeof (a, b), delete (a, b)];\n",
                "}\n",
                "a, b && c, a, b == c, a, b + c, a && (b, c), a == (b, c), ",
                "a + (b, c);\n",
            )
        );
    }

    #[test]
    fn folds_unary_constants_with_upstream_side_effect_guards() {
        let input = "x=+5;x=-5;x=~5;x=!5;x=typeof 5;x=+\"\";x=+[];x=+{};x=+/1/;\
                     x=+[1];x=+\"123\";x=+\"-123\";x=+\"0x10\";\
                     x=+{toString:()=>1};x=+{valueOf:()=>1}";
        assert_eq!(
            code(transform(input, TransformOptions::default())),
            concat!(
                "x = 5;\n",
                "x = -5;\n",
                "x = ~5;\n",
                "x = false;\n",
                "x = \"number\";\n",
                "x = 0;\n",
                "x = 0;\n",
                "x = NaN;\n",
                "x = NaN;\n",
                "x = +[1];\n",
                "x = 123;\n",
                "x = -123;\n",
                "x = +\"0x10\";\n",
                "x = +{ toString: () => 1 };\n",
                "x = +{ valueOf: () => 1 };\n",
            )
        );
        assert_eq!(
            code(transform(
                input,
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "x = 5, x = -5, x = -6, x = !1, x = \"number\", x = 0, x = 0, ",
                "x = NaN, x = NaN, x = +[1], x = 123, x = -123, x = +\"0x10\", ",
                "x = +{ toString: () => 1 }, x = +{ valueOf: () => 1 };\n",
            )
        );
    }

    #[test]
    fn folds_and_shortens_equality_comparisons() {
        for input in [
            "return typeof x !== 'undefined'",
            "return typeof x != 'undefined'",
            "return 'undefined' !== typeof x",
            "return 'undefined' != typeof x",
        ] {
            assert_eq!(
                code(transform(
                    input,
                    TransformOptions {
                        minify_syntax: true,
                        ..TransformOptions::default()
                    }
                )),
                "return typeof x < \"u\";\n",
                "{input}"
            );
        }
        for input in [
            "return typeof x === 'undefined'",
            "return typeof x == 'undefined'",
            "return 'undefined' === typeof x",
            "return 'undefined' == typeof x",
        ] {
            assert_eq!(
                code(transform(
                    input,
                    TransformOptions {
                        minify_syntax: true,
                        ..TransformOptions::default()
                    }
                )),
                "return typeof x > \"u\";\n",
                "{input}"
            );
        }
        for (input, expected) in [
            ("x = 3 == 6", "x = false;\n"),
            ("x = 3 != 6", "x = true;\n"),
            ("x = 3 === 6", "x = false;\n"),
            ("x = 3 !== 6", "x = true;\n"),
        ] {
            assert_eq!(
                code(transform(input, TransformOptions::default())),
                expected,
                "{input}"
            );
        }
        for (input, expected) in [
            ("return +a === 0", "return +a == 0;\n"),
            ("return -a === 0", "return -a === 0;\n"),
            ("return !a === false", "return !!a;\n"),
            ("return x == void 0", "return x == null;\n"),
            ("return void 0 !== x", "return x !== void 0;\n"),
            ("return (a, -1n) !== -1", "return a, -1n !== -1;\n"),
        ] {
            assert_eq!(
                code(transform(
                    input,
                    TransformOptions {
                        minify_syntax: true,
                        ..TransformOptions::default()
                    }
                )),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn inlines_template_primitives_and_preserves_tag_receivers() {
        let options = TransformOptions {
            minify_syntax: true,
            ..TransformOptions::default()
        };
        for (input, expected) in [
            ("_ = `a${x}b${'y'}c`", "_ = `a${x}byc`;\n"),
            ("_ = `a${'x'}b${y}c`", "_ = `axb${y}c`;\n"),
            ("_ = `a${'x'}b${'y'}c`", "_ = \"axbyc\";\n"),
            ("tag`a${x}b${'y'}c`", "tag`a${x}b${\"y\"}c`;\n"),
            ("x.y``", "x.y``;\n"),
            ("x[y]``", "x[y]``;\n"),
            ("(1, x.y)``", "(0, x.y)``;\n"),
            ("(1, x[y])``", "(0, x[y])``;\n"),
            ("(true && x.y)``", "(0, x.y)``;\n"),
            ("(true && x[y])``", "(0, x[y])``;\n"),
            ("(false || x.y)``", "(0, x.y)``;\n"),
            ("(false || x[y])``", "(0, x[y])``;\n"),
            ("(null ?? x.y)``", "(0, x.y)``;\n"),
            ("(null ?? x[y])``", "(0, x[y])``;\n"),
            (
                "function f(a) { let c = a.b; return c`` }",
                "function f(a) {\n  return (0, a.b)``;\n}\n",
            ),
            (
                "function f(a) { let c = a.b; return c`${x}` }",
                "function f(a) {\n  return (0, a.b)`${x}`;\n}\n",
            ),
        ] {
            assert_eq!(code(transform(input, options.clone())), expected, "{input}");
        }
    }

    #[test]
    fn inlines_single_use_locals_with_ordering_guards() {
        assert_eq!(
            code(transform(
                "function chain(){let x=fn();let y=x[prop];let z=y.val;throw z}\
                 function keepThis(arg0){let x=arg0.foo;(0,x)()}\
                 function optional(arg0,arg1){let x=fn();return arg1?.[x]}\
                 function conditional(arg0,arg1){let x=arg0;return (arg1?1:2)?x:3}\
                 function indirect(){let x=eval;x(\"code\")}\
                 function spread(arg0){let x=1;return {...arg0,c:x}}\
                 function dead(){let x=1;if(false)x++;return x}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "function chain() {\n",
                "  throw fn()[prop].val;\n",
                "}\n",
                "function keepThis(arg0) {\n",
                "  let x = arg0.foo;\n",
                "  x();\n",
                "}\n",
                "function optional(arg0, arg1) {\n",
                "  let x = fn();\n",
                "  return arg1?.[x];\n",
                "}\n",
                "function conditional(arg0, arg1) {\n",
                "  return arg0;\n",
                "}\n",
                "function indirect() {\n",
                "  (0, eval)(\"code\");\n",
                "}\n",
                "function spread(arg0) {\n",
                "  return { ...arg0, c: 1 };\n",
                "}\n",
                "function dead() {\n",
                "  return 1;\n",
                "}\n",
            )
        );
    }

    #[test]
    fn cleans_up_terminal_returns_switches_and_empty_loops() {
        assert_eq!(
            code(transform(
                "function terminal(){let x=1;return void x}\
                 function oneCase(arg0){let x=arg0;switch(x){case 0:return 1}}\
                 function emptyLoop(arg0){let x=arg0;do{}while(x)}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "function terminal() {\n",
                "  let x = 1;\n",
                "}\n",
                "function oneCase(arg0) {\n",
                "  if (arg0 === 0)\n",
                "    return 1;\n",
                "}\n",
                "function emptyLoop(arg0) {\n",
                "  let x = arg0;\n",
                "  do\n",
                "    ;\n",
                "  while (x);\n",
                "}\n",
            )
        );
    }

    #[test]
    fn mangles_implicit_and_adjacent_jumps_like_esbuild() {
        assert_eq!(
            code(transform(
                "function chain(){a=b;if(a)return a;if(b)c=b;return c}\
                 function nested(){if(y){if(z)return}}\
                 function empty(x){if(!x.y){}else return x}\
                 function implicit(x){if(!x.y)return undefined;return x}\
                 let arrow=()=>{x();return y};\
                 async function* preserve(){return undefined}\
                 while(x){t();if(y)continue;z()}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "function chain() {\n",
                "  return a = b, a || (b && (c = b), c);\n",
                "}\n",
                "function nested() {\n",
                "  y && z;\n",
                "}\n",
                "function empty(x2) {\n",
                "  if (x2.y)\n",
                "    return x2;\n",
                "}\n",
                "function implicit(x2) {\n",
                "  if (x2.y)\n",
                "    return x2;\n",
                "}\n",
                "let arrow = () => (x(), y);\n",
                "async function* preserve() {\n",
                "  return void 0;\n",
                "}\n",
                "for (; x; )\n",
                "  t(), !y && z();\n",
            )
        );
    }

    #[test]
    fn lowers_annex_b_block_functions_like_esbuild() {
        let options = TransformOptions {
            minify_syntax: true,
            ..TransformOptions::default()
        };
        assert_eq!(
            code(transform(
                "while(x){if(y)continue;function y(){}}\
                 if(flag){function f(){}}use(f);\
                 if(1)function g(){}let g",
                options.clone()
            )),
            concat!(
                "for (; x; ) {\n",
                "  let y2 = function() {\n",
                "  };\n",
                "  var y = y2;\n",
                "}\n",
                "if (flag)\n",
                "  var f = function() {\n",
                "  };\n",
                "use(f);\n",
                "{\n",
                "  let g2 = function() {\n",
                "  };\n",
                "}\n",
                "let g;\n",
            )
        );
        assert_eq!(
            code(transform(
                "\"use strict\";{function f(){}use(f)}",
                options.clone()
            )),
            concat!(
                "\"use strict\";\n",
                "{\n",
                "  let f = function() {\n",
                "  };\n",
                "  use(f);\n",
                "}\n",
            )
        );
        assert_eq!(
            code(transform("{eval(\"\");function f(){}use(f)}", options)),
            concat!(
                "{\n",
                "  function f() {\n",
                "  }\n",
                "  eval(\"\"), use(f);\n",
                "}\n",
            )
        );
    }

    #[test]
    fn removes_overwritten_function_declarations_when_minifying_syntax() {
        assert_eq!(
            code(transform(
                "function f(){x()}function f(){y()}\
                 function g(){x()}function*g(){y()}\
                 async function h(){x()}function h(){y()}\
                 var i;function i(){x()}function i(){y()}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "function f() {\n",
                "  y();\n",
                "}\n",
                "function* g() {\n",
                "  y();\n",
                "}\n",
                "function h() {\n",
                "  y();\n",
                "}\n",
                "var i;\n",
                "function i() {\n",
                "  y();\n",
                "}\n",
            )
        );
    }

    #[test]
    fn folds_constant_logical_branches_and_indents_nested_blocks() {
        assert_eq!(
            code(transform(
                "(function foo(){{var arguments}});\
                 var x=(true&&function(){y()})();\
                 a=false||g;b=null??h;c=1??i;d=(side(),true)&&j",
                TransformOptions::default()
            )),
            concat!(
                "(function foo() {\n",
                "  {\n",
                "    var arguments;\n",
                "  }\n",
                "});\n",
                "var x = (function() {\n",
                "  y();\n",
                "})();\n",
                "a = g;\n",
                "b = h;\n",
                "c = 1;\n",
                "d = (side(), true) && j;\n",
            )
        );
    }

    #[test]
    fn merges_adjacent_and_conditional_throws_like_esbuild() {
        assert_eq!(
            code(transform(
                "function complex(){a=b;if(a)throw a;if(b)c=b;throw c}\
                 function conditional(){if(!a)throw b;throw c}\
                 function branches(){if(!!a)throw b();else throw c()}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "function complex() {\n",
                "  throw a = b, a || (b && (c = b), c);\n",
                "}\n",
                "function conditional() {\n",
                "  throw a ? c : b;\n",
                "}\n",
                "function branches() {\n",
                "  throw a ? b() : c();\n",
                "}\n",
            )
        );
    }

    #[test]
    fn simplifies_undefined_initializers_and_array_spreads_like_esbuild() {
        assert_eq!(
            code(transform(
                "let value=undefined;let {}=undefined;var other=undefined;\
                 x=new foo(1,...[2,...y,3],4);z=[1,...[,2,,],3]",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "let value, {} = void 0;\n",
                "var other = void 0;\n",
                "x = new foo(1, 2, ...y, 3, 4), z = [1, void 0, 2, void 0, 3];\n",
            )
        );
    }

    #[test]
    fn simplifies_object_spreads_like_esbuild() {
        assert_eq!(
            code(transform(
                "x={a,...{},b};y={a,...{b,...c,d},e};\
                 z={a,...{b,get c(){return q++},d},e};\
                 p={a,...true,...void 0,b};keep={a,...void side(),b}",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "x = { a, b }, y = { a, b, ...c, d, e }, z = { a, b, ...{ get c() {\n",
                "  return q++;\n",
                "}, d }, e }, p = { a, b }, keep = { a, ...void side(), b };\n",
            )
        );
    }

    #[test]
    fn folds_object_properties_and_optional_chains_like_esbuild() {
        assert_eq!(
            code(transform(
                "var z;a={y:z}.y;b={foo:/* @__PURE__ */foo(),y:1}.y;\
                 c={__proto__:null}.y;d={y:{z:1}}?.y.z;e={y:{z:1}}?.y?.z;\
                 call={a:fn}.a();construct=new ({a:fn}.a)();\
                 removed=delete ({a:1}.a);tagged={a:tag}.a``",
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "var z;\n",
                "a = z, b = 1, c = void 0, d = 1, e = { z: 1 }?.z, ",
                "call = { a: fn }.a(), construct = new { a: fn }.a(), ",
                "removed = delete 1, tagged = { a: tag }.a``;\n",
            )
        );
    }

    #[test]
    fn minifies_booleans_and_jsx_object_spreads_like_esbuild() {
        assert_eq!(
            code(transform(
                "x=true;y=false;z=true**n;\
                 jsx=<foo bar {...{}}/>;jsx2=<foo bar {...{bar}}/>",
                TransformOptions {
                    loader: Loader::Jsx,
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            concat!(
                "x = !0, y = !1, z = (!0) ** n, ",
                "jsx = /* @__PURE__ */ React.createElement(\"foo\", { bar: !0 }), ",
                "jsx2 = /* @__PURE__ */ React.createElement(\"foo\", { bar: !0, bar });\n",
            )
        );
    }

    #[test]
    fn minifies_return_keyword_spacing_like_esbuild() {
        assert_eq!(
            code(transform(
                "function a(x){return [x]}function b(x){return {x}}function c(){return \"x\"}function d(){return /x/}function e(x){return !x}function f(x){return typeof x}",
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "function a(x){return[x]}function b(x){return{x}}function c(){return\"x\"}function d(){return/x/}function e(x){return!x}function f(x){return typeof x}\n"
        );
    }

    #[test]
    fn minifies_function_arguments_and_enum_slots_like_esbuild() {
        assert_eq!(
            code(transform(
                "enum LongName{LongName=0,Other=LongName};function f(x,y){return [LongName,x,y]}",
                TransformOptions {
                    loader: Loader::Ts,
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "var LongName=(n=>(n[n.LongName=0]=\"LongName\",n[n.Other=0]=\"Other\",n))(LongName||{});function f(e,r){return[LongName,e,r]}\n"
        );
        assert_eq!(
            code(transform(
                "function f(arguments){return arguments}function g(){return arguments}",
                TransformOptions {
                    minify_syntax: true,
                    minify_identifiers: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "function f(n){return n}function g(){return arguments}\n"
        );
    }

    #[test]
    fn preserves_template_literals_without_syntax_minification() {
        let input = "function f(){return `x`}";
        assert_eq!(
            code(transform(input, TransformOptions::default())),
            "function f() {\n  return `x`;\n}\n"
        );
        assert_eq!(
            code(transform(
                input,
                TransformOptions {
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "function f(){return`x`}\n"
        );
        assert_eq!(
            code(transform(
                input,
                TransformOptions {
                    minify_syntax: true,
                    ..TransformOptions::default()
                }
            )),
            "function f() {\n  return \"x\";\n}\n"
        );
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
    fn derives_rebecca_purple_compatibility_from_browser_targets() {
        let transform_for_ie = |version: &str| {
            code(transform(
                "a { color: ReBeCcApUrPlE }",
                TransformOptions {
                    loader: Loader::Css,
                    engines: vec![Engine {
                        name: EngineName::Ie,
                        version: version.into(),
                    }],
                    ..TransformOptions::default()
                },
            ))
        };
        assert_eq!(transform_for_ie("10"), "a {\n  color: #663399;\n}\n");
        assert_eq!(transform_for_ie("11"), "a {\n  color: ReBeCcApUrPlE;\n}\n");
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
    fn derives_hex_rgba_compatibility_from_browser_targets() {
        let transform_for_chrome = |version: &str| {
            code(transform(
                "a { color: #1234 }",
                TransformOptions {
                    loader: Loader::Css,
                    engines: vec![Engine {
                        name: EngineName::Chrome,
                        version: version.into(),
                    }],
                    ..TransformOptions::default()
                },
            ))
        };
        assert_eq!(
            transform_for_chrome("61"),
            "a {\n  color: rgba(17, 34, 51, .267);\n}\n"
        );
        assert_eq!(transform_for_chrome("62"), "a {\n  color: #1234;\n}\n");
    }

    #[test]
    fn lowers_hwb_for_unsupported_browser_targets_and_overrides() {
        let input = "a { color: hwb(90deg 20% 40%); outline-color: hwb(.75turn 20% 40% / .75) }";
        let lowered = "a {\n\
                       \x20\x20color: #669933;\n\
                       \x20\x20outline-color: #663399bf;\n\
                       }\n";
        let preserved = "a {\n\
                         \x20\x20color: hwb(90deg 20% 40%);\n\
                         \x20\x20outline-color: hwb(.75turn 20% 40% / .75);\n\
                         }\n";

        let transform_for_chrome = |version: &str, supported| {
            code(transform(
                input,
                TransformOptions {
                    loader: Loader::Css,
                    engines: vec![Engine {
                        name: EngineName::Chrome,
                        version: version.into(),
                    }],
                    supported,
                    ..TransformOptions::default()
                },
            ))
        };

        assert_eq!(transform_for_chrome("100", HashMap::new()), lowered);
        assert_eq!(transform_for_chrome("101", HashMap::new()), preserved);
        assert_eq!(
            transform_for_chrome("100", HashMap::from([("hwb".into(), true)])),
            preserved
        );
        assert_eq!(
            transform_for_chrome("101", HashMap::from([("hwb".into(), false)])),
            lowered
        );
    }

    #[test]
    fn lowers_inset_for_unsupported_browser_targets() {
        let explicit = code(transform(
            "a { inset: 1px 2px }",
            TransformOptions {
                loader: Loader::Css,
                supported: HashMap::from([("inset-property".into(), false)]),
                ..TransformOptions::default()
            },
        ));
        assert_eq!(
            explicit,
            "a {\n\
             \x20\x20top: 1px;\n\
             \x20\x20right: 2px;\n\
             \x20\x20bottom: 1px;\n\
             \x20\x20left: 2px;\n\
             }\n"
        );

        let transform_for_chrome = |version: &str| {
            code(transform(
                "a { inset: 1px 2px }",
                TransformOptions {
                    loader: Loader::Css,
                    engines: vec![Engine {
                        name: EngineName::Chrome,
                        version: version.into(),
                    }],
                    ..TransformOptions::default()
                },
            ))
        };
        assert_eq!(transform_for_chrome("86"), explicit);
        assert_eq!(transform_for_chrome("87"), "a {\n  inset: 1px 2px;\n}\n");
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
    fn lowers_unsupported_css_gradient_double_positions() {
        let input =
            "a { background: linear-gradient(red calc(10%) calc(20%), yellow 70% 80%, blue) }";
        let lowered = code(transform(
            input,
            TransformOptions {
                loader: Loader::Css,
                supported: HashMap::from([("gradient-double-position".into(), false)]),
                ..TransformOptions::default()
            },
        ));
        assert!(lowered.contains("red calc(10%),"), "{lowered}");
        assert!(lowered.contains("red calc(20%),"), "{lowered}");
        assert!(
            lowered.contains("yellow 70%,\n      yellow 80%"),
            "{lowered}"
        );

        let supported = code(transform(
            input,
            TransformOptions {
                loader: Loader::Css,
                supported: HashMap::from([("gradient-double-position".into(), true)]),
                ..TransformOptions::default()
            },
        ));
        assert!(supported.contains("red calc(10%) calc(20%)"), "{supported}");
        assert!(supported.contains("yellow 70% 80%"), "{supported}");
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
