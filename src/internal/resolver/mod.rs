//! Port of upstream `internal/resolver`.

use std::any::Any;
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::internal::{
    config::{
        ExternalMatchers, ExternalSettings, MaybeBool, Platform, TsAlwaysStrict, TsConfig,
        TsConfigJsx, TsImportsNotUsedAsValues, TsJsx, TsTarget,
    },
    fs::{DifferentCase, EntryKind, Fs},
    helpers::{is_inside_node_modules, utf16_to_string},
    js_ast::{Expr, ExprData, ModuleType, ModuleTypeData},
    js_lexer::{JsonFlavor, range_of_identifier},
    js_parser::{JsonOptions, parse_json},
    logger::{
        LineColumnTracker, Loc, Log, Msg, MsgData, MsgId, MsgKind, Path, PathFlags, Range, Source,
    },
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedPath {
    pub path: String,
    pub different_case: Option<DifferentCase>,
    pub disabled: bool,
}

#[must_use]
pub fn load_as_file(
    file_system: &dyn Fs,
    path: &str,
    extension_order: &[String],
) -> Option<LoadedPath> {
    let directory = file_system.dir(path);
    let (entries, error, _) = file_system.read_directory(&directory);
    if error.is_some() {
        return None;
    }
    let base = file_system.base(path);
    let try_file = |candidate: &str| {
        let (entry, different_case) = entries.get(candidate);
        entry
            .filter(|entry| entry.kind(file_system) == EntryKind::File)
            .map(|_| LoadedPath {
                path: file_system.join(&[&directory, candidate]),
                different_case,
                disabled: false,
            })
    };

    if let Some(result) = try_file(&base) {
        return Some(result);
    }
    for extension in extension_order {
        if let Some(result) = try_file(&format!("{base}{extension}")) {
            return Some(result);
        }
    }
    for (old_extension, rewritten_extensions) in [
        (".js", &[".ts", ".tsx"][..]),
        (".jsx", &[".ts", ".tsx"][..]),
        (".mjs", &[".mts"][..]),
        (".cjs", &[".cts"][..]),
    ] {
        let Some(without_extension) = base.strip_suffix(old_extension) else {
            continue;
        };
        for extension in rewritten_extensions {
            if let Some(result) = try_file(&format!("{without_extension}{extension}")) {
                return Some(result);
            }
        }
        break;
    }
    None
}

#[must_use]
pub fn load_as_index(
    file_system: &dyn Fs,
    path: &str,
    extension_order: &[String],
) -> Option<LoadedPath> {
    let (entries, error, _) = file_system.read_directory(path);
    if error.is_some() {
        return None;
    }
    for extension in extension_order {
        let candidate = format!("index{extension}");
        let (entry, different_case) = entries.get(&candidate);
        if entry.is_some_and(|entry| entry.kind(file_system) == EntryKind::File) {
            return Some(LoadedPath {
                path: file_system.join(&[path, &candidate]),
                different_case,
                disabled: false,
            });
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct LoadedPathPair {
    pub paths: PathPair,
    pub different_case: Option<DifferentCase>,
}

#[allow(clippy::too_many_arguments)]
pub fn load_as_directory(
    log: &Log,
    file_system: &dyn Fs,
    path: &str,
    extension_order: &[String],
    platform: Platform,
    configured_main_fields: Option<&[String]>,
    is_require: bool,
) -> Option<LoadedPathPair> {
    let (entries, error, _) = file_system.read_directory(path);
    if error.is_some() {
        return None;
    }
    let package = entries
        .get("package.json")
        .0
        .filter(|entry| entry.kind(file_system) == EntryKind::File)
        .and_then(|_| {
            let package_path = file_system.join(&[path, "package.json"]);
            let (contents, error, _) = file_system.read_file(&package_path);
            if error.is_some() {
                return None;
            }
            let source = Source {
                key_path: Path {
                    text: package_path,
                    namespace: "file".into(),
                    ..Path::default()
                },
                contents: Arc::from(contents.into_bytes()),
                ..Source::default()
            };
            parse_package_json(
                log,
                &source,
                path,
                file_system,
                platform,
                configured_main_fields,
            )
        });

    if let Some(package) = &package {
        let defaults: &[&str] = match platform {
            Platform::Browser => &["browser", "module", "main"],
            Platform::Node => &["main", "module"],
            Platform::Neutral => &[],
        };
        let keys: Vec<&str> = configured_main_fields.map_or_else(
            || defaults.to_vec(),
            |fields| fields.iter().map(String::as_str).collect(),
        );
        let automatic = configured_main_fields.is_none();
        for key in keys {
            let Some(main) = package.main_fields.get(key) else {
                continue;
            };
            let Some(primary) = load_package_main_candidate(
                file_system,
                path,
                package,
                &main.relative_path,
                extension_order,
            ) else {
                continue;
            };
            if automatic && key == "module" {
                let secondary = package
                    .main_fields
                    .get("main")
                    .and_then(|main| {
                        load_package_main_candidate(
                            file_system,
                            path,
                            package,
                            &main.relative_path,
                            extension_order,
                        )
                    })
                    .or_else(|| load_as_index(file_system, path, extension_order));
                if let Some(secondary) = secondary {
                    if is_require {
                        return Some(LoadedPathPair {
                            paths: file_path_pair(&secondary.path, secondary.disabled),
                            different_case: secondary.different_case,
                        });
                    }
                    return Some(LoadedPathPair {
                        paths: PathPair {
                            primary: file_path(&primary.path, primary.disabled),
                            secondary: file_path(&secondary.path, secondary.disabled),
                            ..PathPair::default()
                        },
                        different_case: primary.different_case,
                    });
                }
            }
            return Some(LoadedPathPair {
                paths: file_path_pair(&primary.path, primary.disabled),
                different_case: primary.different_case,
            });
        }
    }

    load_as_index(file_system, path, extension_order).map(|loaded| LoadedPathPair {
        paths: file_path_pair(&loaded.path, loaded.disabled),
        different_case: loaded.different_case,
    })
}

fn load_package_main_candidate(
    file_system: &dyn Fs,
    package_dir: &str,
    package: &PackageJson,
    relative_path: &str,
    extension_order: &[String],
) -> Option<LoadedPath> {
    let mut mapped_path = relative_path.to_string();
    if let Some(remapped) = package.browser_map.get(relative_path).or_else(|| {
        relative_path
            .strip_prefix("./")
            .and_then(|path| package.browser_map.get(path))
    }) {
        let Some(remapped) = remapped else {
            return Some(LoadedPath {
                path: file_system.join(&[package_dir, relative_path]),
                different_case: None,
                disabled: true,
            });
        };
        mapped_path.clone_from(remapped);
    }
    let absolute = file_system.join(&[package_dir, &mapped_path]);
    load_as_file(file_system, &absolute, extension_order)
        .or_else(|| load_as_index(file_system, &absolute, extension_order))
}

fn file_path(text: &str, disabled: bool) -> Path {
    Path {
        text: text.to_string(),
        namespace: "file".into(),
        flags: if disabled {
            PathFlags::DISABLED
        } else {
            PathFlags::default()
        },
        ..Path::default()
    }
}

fn file_path_pair(text: &str, disabled: bool) -> PathPair {
    PathPair {
        primary: file_path(text, disabled),
        ..PathPair::default()
    }
}

#[must_use]
pub fn is_package_path(path: &str) -> bool {
    !path.starts_with('/')
        && !path.starts_with("./")
        && !path.starts_with("../")
        && path != "."
        && path != ".."
}

pub const BUILT_IN_NODE_MODULES: &[&str] = &[
    "_http_agent",
    "_http_client",
    "_http_common",
    "_http_incoming",
    "_http_outgoing",
    "_http_server",
    "_stream_duplex",
    "_stream_passthrough",
    "_stream_readable",
    "_stream_transform",
    "_stream_wrap",
    "_stream_writable",
    "_tls_common",
    "_tls_wrap",
    "assert",
    "assert/strict",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "sys",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

#[must_use]
pub fn is_node_builtin(path: &str) -> bool {
    BUILT_IN_NODE_MODULES.binary_search(&path).is_ok()
}

#[derive(Clone, Copy, Default)]
pub struct ResolverContext<'a> {
    pub tsconfig: Option<&'a TsConfigJson>,
    pub pnp: Option<&'a PnpData>,
    pub external_settings: Option<&'a ExternalSettings>,
    pub external_packages: bool,
    pub conditions: Option<&'a [String]>,
    pub package_aliases: Option<&'a HashMap<String, String>>,
    pub strip_node_prefix_for_import: bool,
    pub strip_node_prefix_for_require: bool,
}

fn is_external_match(matchers: &ExternalMatchers, path: &str) -> bool {
    matchers.exact.contains_key(path)
        || matchers
            .patterns
            .iter()
            .any(|pattern| path.starts_with(&pattern.prefix) && path.ends_with(&pattern.suffix))
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_file_or_package(
    log: &Log,
    file_system: &dyn Fs,
    source_dir: &str,
    import_path: &str,
    extension_order: &[String],
    platform: Platform,
    configured_main_fields: Option<&[String]>,
    is_require: bool,
) -> Option<LoadedPathPair> {
    resolve_file_or_package_with_context(
        log,
        file_system,
        source_dir,
        import_path,
        extension_order,
        platform,
        configured_main_fields,
        is_require,
        ResolverContext::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_file_or_package_with_context(
    log: &Log,
    file_system: &dyn Fs,
    source_dir: &str,
    import_path: &str,
    extension_order: &[String],
    platform: Platform,
    configured_main_fields: Option<&[String]>,
    is_require: bool,
    context: ResolverContext<'_>,
) -> Option<LoadedPathPair> {
    resolve_file_or_package_core(
        log,
        file_system,
        source_dir,
        import_path,
        extension_order,
        platform,
        configured_main_fields,
        is_require,
        context,
        false,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn resolve_file_or_package_core(
    log: &Log,
    file_system: &dyn Fs,
    source_dir: &str,
    import_path: &str,
    extension_order: &[String],
    platform: Platform,
    configured_main_fields: Option<&[String]>,
    is_require: bool,
    context: ResolverContext<'_>,
    forbid_package_imports: bool,
) -> Option<LoadedPathPair> {
    let mut source_dir = Cow::Borrowed(source_dir);
    let mut import_path = Cow::Borrowed(import_path);
    if is_package_path(&import_path)
        && let Some(aliases) = context.package_aliases
    {
        let mut matched: Option<(&str, &str)> = None;
        for (key, value) in aliases {
            if import_path.starts_with(key)
                && (import_path.len() == key.len()
                    || import_path.as_bytes().get(key.len()) == Some(&b'/'))
                && matched.is_none_or(|(old, _)| key.len() > old.len())
            {
                matched = Some((key, value));
            }
        }
        if let Some((key, value)) = matched {
            let tail = &import_path[key.len()..];
            import_path = Cow::Owned(if tail == "/" {
                value.to_string()
            } else {
                format!("{value}{tail}")
            });
            source_dir = Cow::Owned(file_system.cwd().to_string());
        }
    }
    let source_dir = source_dir.as_ref();
    let import_path = import_path.as_ref();

    if context
        .external_settings
        .is_some_and(|settings| is_external_match(&settings.pre_resolve, import_path))
        || (context.external_packages && is_package_path(import_path))
    {
        return Some(LoadedPathPair {
            paths: PathPair {
                primary: Path {
                    text: import_path.to_string(),
                    ..Path::default()
                },
                is_external: true,
                ..PathPair::default()
            },
            different_case: None,
        });
    }
    if platform == Platform::Node
        && (is_node_builtin(import_path) || import_path.starts_with("node:"))
    {
        let strip_prefix = import_path.starts_with("node:")
            && if is_require {
                context.strip_node_prefix_for_require
            } else {
                context.strip_node_prefix_for_import
            };
        let path = if strip_prefix {
            import_path.strip_prefix("node:").unwrap_or(import_path)
        } else {
            import_path
        };
        return Some(LoadedPathPair {
            paths: PathPair {
                primary: Path {
                    text: path.to_string(),
                    ..Path::default()
                },
                is_external: true,
                ..PathPair::default()
            },
            different_case: None,
        });
    }
    if let Some(tsconfig) = context.tsconfig {
        if let Some(candidates) = match_tsconfig_path_candidates(tsconfig, import_path, file_system)
        {
            for candidate in candidates {
                if let Some(result) = load_as_file_or_directory(
                    log,
                    file_system,
                    &candidate.path,
                    extension_order,
                    platform,
                    configured_main_fields,
                    is_require,
                ) {
                    return Some(result);
                }
            }
        }
        if is_package_path(import_path)
            && let Some(base_url) = &tsconfig.base_url
        {
            let base_path = file_system.join(&[base_url, import_path]);
            if let Some(result) = load_as_file_or_directory(
                log,
                file_system,
                &base_path,
                extension_order,
                platform,
                configured_main_fields,
                is_require,
            ) {
                return Some(result);
            }
        }
    }
    if import_path.starts_with('/') || file_system.is_abs(import_path) {
        return load_as_file_or_directory(
            log,
            file_system,
            import_path,
            extension_order,
            platform,
            configured_main_fields,
            is_require,
        );
    }
    if !is_package_path(import_path) {
        let absolute = file_system.join(&[source_dir, import_path]);
        return load_as_file_or_directory(
            log,
            file_system,
            &absolute,
            extension_order,
            platform,
            configured_main_fields,
            is_require,
        );
    }

    if import_path.starts_with('#') && !forbid_package_imports {
        let mut current = source_dir.to_string();
        loop {
            if let Some(package) =
                read_package_json(log, file_system, &current, platform, configured_main_fields)
                && let Some(imports) = &package.imports_map
            {
                let resolution = handle_package_map_post_conditions(resolve_package_imports(
                    import_path,
                    &imports.root,
                    &package_conditions(platform, is_require, context.conditions),
                ));
                if resolution.status == PackageMapStatus::PackageResolve {
                    return resolve_file_or_package_core(
                        log,
                        file_system,
                        &current,
                        &resolution.path,
                        extension_order,
                        platform,
                        configured_main_fields,
                        is_require,
                        context,
                        true,
                    );
                }
                return finalize_package_map_resolution(
                    log,
                    file_system,
                    &current,
                    &resolution,
                    extension_order,
                    platform,
                    configured_main_fields,
                    is_require,
                );
            }
            let parent = file_system.dir(&current);
            if parent == current {
                break;
            }
            current = parent;
        }
        return None;
    }

    if let Some(pnp) = context.pnp {
        let result = pnp.resolve_to_unqualified(import_path, source_dir, file_system);
        if result.status.is_error() {
            return None;
        }
        if result.status == PnpStatus::Success {
            let absolute = file_system.join(&[&result.package_dir_path, &result.package_subpath]);
            if let Some(package) = read_package_json(
                log,
                file_system,
                &result.package_dir_path,
                platform,
                configured_main_fields,
            ) && let Some(exports) = &package.exports_map
            {
                let resolution = handle_package_map_post_conditions(resolve_package_exports(
                    "/",
                    &format!(".{}", result.package_subpath),
                    &exports.root,
                    &package_conditions(platform, is_require, context.conditions),
                ));
                return finalize_package_map_resolution(
                    log,
                    file_system,
                    &result.package_dir_path,
                    &resolution,
                    extension_order,
                    platform,
                    configured_main_fields,
                    is_require,
                );
            }
            return load_as_file_or_directory(
                log,
                file_system,
                &absolute,
                extension_order,
                platform,
                configured_main_fields,
                is_require,
            );
        }
    }

    let (package_name, package_subpath) = parse_esm_package_name(import_path)?;
    let mut current = source_dir.to_string();
    loop {
        if file_system.base(&current) != "node_modules" {
            let node_modules = file_system.join(&[&current, "node_modules"]);
            let package_dir = file_system.join(&[&node_modules, package_name]);
            if let Some(package) = read_package_json(
                log,
                file_system,
                &package_dir,
                platform,
                configured_main_fields,
            ) && let Some(exports) = &package.exports_map
            {
                let resolution = handle_package_map_post_conditions(resolve_package_exports(
                    "/",
                    &package_subpath,
                    &exports.root,
                    &package_conditions(platform, is_require, context.conditions),
                ));
                return finalize_package_map_resolution(
                    log,
                    file_system,
                    &package_dir,
                    &resolution,
                    extension_order,
                    platform,
                    configured_main_fields,
                    is_require,
                );
            }

            let absolute = file_system.join(&[&node_modules, import_path]);
            if let Some(result) = load_as_file_or_directory(
                log,
                file_system,
                &absolute,
                extension_order,
                platform,
                configured_main_fields,
                is_require,
            ) {
                return Some(result);
            }
        }
        let parent = file_system.dir(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    None
}

fn package_conditions(
    platform: Platform,
    is_require: bool,
    custom: Option<&[String]>,
) -> HashMap<String, bool> {
    let mut conditions = HashMap::from([
        ("default".into(), true),
        (if is_require { "require" } else { "import" }.into(), true),
        (
            match platform {
                Platform::Browser => "browser",
                Platform::Node => "node",
                Platform::Neutral => "default",
            }
            .into(),
            true,
        ),
    ]);
    if let Some(custom) = custom {
        conditions.extend(custom.iter().cloned().map(|condition| (condition, true)));
    }
    conditions
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_with_metadata(
    log: &Log,
    file_system: &dyn Fs,
    source_dir: &str,
    import_path: &str,
    extension_order: &[String],
    platform: Platform,
    configured_main_fields: Option<&[String]>,
    is_require: bool,
    context: ResolverContext<'_>,
) -> Option<ResolveResult> {
    let loaded = resolve_file_or_package_with_context(
        log,
        file_system,
        source_dir,
        import_path,
        extension_order,
        platform,
        configured_main_fields,
        is_require,
        context,
    )?;
    let mut result = ResolveResult {
        path_pair: loaded.paths,
        different_case: loaded.different_case,
        ..ResolveResult::default()
    };
    if !result.path_pair.is_external
        && context.external_settings.is_some_and(|settings| {
            is_external_match(&settings.post_resolve, &result.path_pair.primary.text)
        })
    {
        result.path_pair.is_external = true;
    }
    if let Some(tsconfig) = context.tsconfig {
        result.ts_config_jsx = tsconfig.jsx_settings.clone();
        result.ts_config = Some(tsconfig.settings);
        result.ts_always_strict = tsconfig.ts_always_strict_or_strict().cloned();
    }
    if platform == Platform::Node
        && is_node_builtin(import_path.strip_prefix("node:").unwrap_or(import_path))
    {
        result.primary_side_effects_data = Some(SideEffectsData::default());
    }
    if result.path_pair.is_external
        || result.path_pair.primary.is_disabled()
        || result.path_pair.primary.namespace != "file"
    {
        return Some(result);
    }

    let resolved_path = result.path_pair.primary.text.replace('\\', "/");
    let mut directory = file_system.dir(&result.path_pair.primary.text);
    loop {
        if let Some(package) = read_package_json(
            log,
            file_system,
            &directory,
            platform,
            configured_main_fields,
        ) {
            result.module_type_data = package.module_type_data.clone();
            if let Some(side_effects_map) = &package.side_effects_map {
                let has_side_effects = side_effects_map.contains_key(&resolved_path)
                    || package
                        .side_effects_regexps
                        .iter()
                        .any(|regexp| regexp.is_match(&resolved_path));
                if !has_side_effects {
                    result
                        .primary_side_effects_data
                        .clone_from(&package.side_effects_data);
                }
            }
            break;
        }
        let parent = file_system.dir(&directory);
        if parent == directory {
            break;
        }
        directory = parent;
    }
    Some(result)
}

#[allow(clippy::too_many_arguments)]
fn load_as_file_or_directory(
    log: &Log,
    file_system: &dyn Fs,
    path: &str,
    extension_order: &[String],
    platform: Platform,
    configured_main_fields: Option<&[String]>,
    is_require: bool,
) -> Option<LoadedPathPair> {
    if let Some(file) = load_as_file(file_system, path, extension_order) {
        return Some(LoadedPathPair {
            paths: file_path_pair(&file.path, file.disabled),
            different_case: file.different_case,
        });
    }
    load_as_directory(
        log,
        file_system,
        path,
        extension_order,
        platform,
        configured_main_fields,
        is_require,
    )
}

fn read_package_json(
    log: &Log,
    file_system: &dyn Fs,
    package_dir: &str,
    platform: Platform,
    configured_main_fields: Option<&[String]>,
) -> Option<PackageJson> {
    let package_path = file_system.join(&[package_dir, "package.json"]);
    let (contents, error, _) = file_system.read_file(&package_path);
    if error.is_some() {
        return None;
    }
    let source = Source {
        key_path: Path {
            text: package_path,
            namespace: "file".into(),
            ..Path::default()
        },
        contents: Arc::from(contents.into_bytes()),
        ..Source::default()
    };
    parse_package_json(
        log,
        &source,
        package_dir,
        file_system,
        platform,
        configured_main_fields,
    )
}

#[allow(clippy::too_many_arguments)]
fn finalize_package_map_resolution(
    log: &Log,
    file_system: &dyn Fs,
    package_dir: &str,
    resolution: &PackageMapResolution,
    extension_order: &[String],
    platform: Platform,
    configured_main_fields: Option<&[String]>,
    is_require: bool,
) -> Option<LoadedPathPair> {
    let relative = resolution
        .path
        .strip_prefix('/')
        .unwrap_or(&resolution.path);
    let absolute = file_system.join(&[package_dir, relative]);
    match resolution.status {
        PackageMapStatus::Exact | PackageMapStatus::ExactEndsWithStar => {
            load_as_file(file_system, &absolute, &[]).map(|file| LoadedPathPair {
                paths: file_path_pair(&file.path, file.disabled),
                different_case: file.different_case,
            })
        }
        PackageMapStatus::Inexact => load_as_file_or_directory(
            log,
            file_system,
            &absolute,
            extension_order,
            platform,
            configured_main_fields,
            is_require,
        ),
        _ => None,
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

pub struct PackageJson {
    pub name: String,
    pub main_fields: HashMap<String, MainField>,
    pub module_type_data: ModuleTypeData,
    pub tsconfig: String,
    pub browser_map: HashMap<String, Option<String>>,
    pub side_effects_map: Option<HashMap<String, bool>>,
    pub side_effects_regexps: Vec<regex::Regex>,
    pub side_effects_data: Option<SideEffectsData>,
    pub imports_map: Option<PackageMap>,
    pub exports_map: Option<PackageMap>,
    pub source: Source,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MainField {
    pub relative_path: String,
    pub key_loc: Loc,
}

#[allow(clippy::too_many_lines)]
pub fn parse_package_json(
    log: &Log,
    source: &Source,
    package_dir: &str,
    file_system: &dyn Fs,
    platform: Platform,
    configured_main_fields: Option<&[String]>,
) -> Option<PackageJson> {
    let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
    if !ok {
        return None;
    }
    let mut tracker = LineColumnTracker::new(Some(source));
    let mut package = PackageJson {
        name: String::new(),
        main_fields: HashMap::new(),
        module_type_data: ModuleTypeData::default(),
        tsconfig: String::new(),
        browser_map: HashMap::new(),
        side_effects_map: None,
        side_effects_regexps: Vec::new(),
        side_effects_data: None,
        imports_map: None,
        exports_map: None,
        source: source.clone(),
    };
    package.name = get_property(&json, "name")
        .and_then(|(value, _)| get_string(value))
        .unwrap_or_default();
    if let Some((value, key_loc)) = get_property(&json, "type") {
        if let Some(text) = get_string(value) {
            let module_type = match text.as_str() {
                "commonjs" => Some(ModuleType::CommonJsPackageJson),
                "module" => Some(ModuleType::EsmPackageJson),
                _ => None,
            };
            if let Some(module_type) = module_type {
                package.module_type_data = ModuleTypeData {
                    source: Some(Box::new(source.clone())),
                    range: source.range_of_string(value.loc),
                    module_type,
                };
            } else {
                let mut notes = vec![MsgData {
                    text: "The \"type\" field must be set to either \"commonjs\" or \"module\"."
                        .into(),
                    ..MsgData::default()
                }];
                let kind = if text.ends_with(".d.ts") {
                    notes[0] = tracker.msg_data(
                        source.range_of_string(key_loc),
                        "TypeScript type declarations use the \"types\" field, not the \"type\" field:",
                    );
                    if let Some(location) = &mut notes[0].location {
                        location.suggestion = "\"types\"".into();
                    }
                    if is_inside_node_modules(&source.key_path.text) {
                        MsgKind::Debug
                    } else {
                        MsgKind::Warning
                    }
                } else {
                    MsgKind::Warning
                };
                log.add_id_with_notes(
                    MsgId::PackageJsonInvalidType,
                    kind,
                    Some(&mut tracker),
                    source.range_of_string(value.loc),
                    format!("{text:?} is not a valid value for the \"type\" field"),
                    notes,
                );
            }
        } else {
            log.add_id(
                MsgId::PackageJsonInvalidType,
                MsgKind::Warning,
                Some(&mut tracker),
                Range {
                    loc: value.loc,
                    ..Range::default()
                },
                "The value for \"type\" must be a string",
            );
        }
    }
    package.tsconfig = get_property(&json, "tsconfig")
        .and_then(|(value, _)| get_string(value))
        .unwrap_or_default();

    let defaults: &[&str] = match platform {
        Platform::Browser => &["browser", "module", "main"],
        Platform::Node => &["main", "module"],
        Platform::Neutral => &[],
    };
    let main_fields: Vec<&str> = configured_main_fields.map_or_else(
        || defaults.to_vec(),
        |fields| fields.iter().map(String::as_str).collect(),
    );
    for field in main_fields.into_iter().chain(["main", "module"]) {
        if package.main_fields.contains_key(field) {
            continue;
        }
        if let Some((value, key_loc)) = get_property(&json, field)
            && let Some(path) = get_string(value)
            && !path.is_empty()
        {
            package.main_fields.insert(
                field.to_string(),
                MainField {
                    relative_path: path,
                    key_loc,
                },
            );
        }
    }
    if platform == Platform::Browser
        && let Some((value, _)) = get_property(&json, "browser")
        && let Some(ExprData::Object(object)) = value.data.as_deref()
    {
        for property in &object.properties {
            let Some(key) = get_string(&property.key) else {
                continue;
            };
            if let Some(replacement) = get_string(&property.value_or_nil) {
                package.browser_map.insert(key, Some(replacement));
            } else if get_bool(&property.value_or_nil) == Some(false) {
                package.browser_map.insert(key, None);
            } else {
                log.add_id(
                    MsgId::PackageJsonInvalidBrowser,
                    MsgKind::Warning,
                    Some(&mut tracker),
                    Range {
                        loc: property.value_or_nil.loc,
                        ..Range::default()
                    },
                    "Each \"browser\" mapping must be a string or a boolean",
                );
            }
        }
    }
    if let Some((value, key_loc)) = get_property(&json, "sideEffects") {
        match value.data.as_deref() {
            Some(ExprData::Boolean(false)) => {
                package.side_effects_map = Some(HashMap::new());
                package.side_effects_data = Some(SideEffectsData {
                    source: Some(source.clone()),
                    range: source.range_of_string(key_loc),
                    ..SideEffectsData::default()
                });
            }
            Some(ExprData::Array(array)) => {
                package.side_effects_map = Some(HashMap::new());
                package.side_effects_data = Some(SideEffectsData {
                    source: Some(source.clone()),
                    range: source.range_of_string(key_loc),
                    is_side_effects_array_in_json: true,
                    ..SideEffectsData::default()
                });
                for item in &array.items {
                    let Some(mut pattern) = get_string(item) else {
                        log.add_id(
                            MsgId::PackageJsonInvalidSideEffects,
                            MsgKind::Warning,
                            Some(&mut tracker),
                            Range {
                                loc: item.loc,
                                ..Range::default()
                            },
                            "Expected string in array for \"sideEffects\"",
                        );
                        continue;
                    };
                    if !pattern.contains('/') {
                        pattern = format!("**/{pattern}");
                    }
                    let absolute = file_system
                        .join(&[package_dir, &pattern])
                        .replace('\\', "/");
                    let (regexp, wildcard) = globstar_to_escaped_regexp(&absolute);
                    if wildcard {
                        if let Ok(regexp) = regex::Regex::new(&regexp) {
                            package.side_effects_regexps.push(regexp);
                        }
                    } else if let Some(map) = &mut package.side_effects_map {
                        map.insert(absolute, true);
                    }
                }
            }
            Some(ExprData::Boolean(true)) => {}
            _ => log.add_id(
                MsgId::PackageJsonInvalidSideEffects,
                MsgKind::Warning,
                Some(&mut tracker),
                Range {
                    loc: value.loc,
                    ..Range::default()
                },
                "The value for \"sideEffects\" must be a boolean or an array",
            ),
        }
    }
    if let Some((value, key_loc)) = get_property(&json, "imports") {
        package.imports_map = parse_imports_exports_map(source, log, value, "imports", key_loc);
        if let Some(map) = &package.imports_map
            && map.root.kind != PackageMapKind::Object
        {
            log.add_id(
                MsgId::PackageJsonInvalidImportsOrExports,
                MsgKind::Warning,
                Some(&mut tracker),
                map.root.first_token,
                "The value for \"imports\" must be an object",
            );
        }
    }
    if let Some((value, key_loc)) = get_property(&json, "exports") {
        package.exports_map = parse_imports_exports_map(source, log, value, "exports", key_loc);
    }
    Some(package)
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PackageMapStatus {
    #[default]
    Undefined,
    UndefinedNoConditionsMatch,
    Null,
    Exact,
    ExactEndsWithStar,
    Inexact,
    PackageResolve,
    InvalidModuleSpecifier,
    InvalidPackageConfiguration,
    InvalidPackageTarget,
    PackagePathNotExported,
    PackageImportNotDefined,
    ModuleNotFound,
    ModuleNotFoundMissingExtension,
    UnsupportedDirectoryImport,
    UnsupportedDirectoryImportMissingIndex,
}

impl PackageMapStatus {
    #[must_use]
    pub const fn is_undefined(self) -> bool {
        matches!(self, Self::Undefined | Self::UndefinedNoConditionsMatch)
    }
}

#[derive(Clone, Debug, Default)]
pub struct PackageMapDebug {
    pub invalid_because: String,
    pub unmatched_conditions: Vec<crate::internal::logger::Span>,
    pub token: Range,
    pub is_because_of_null_literal: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PackageMapResolution {
    pub path: String,
    pub status: PackageMapStatus,
    pub debug: PackageMapDebug,
}

#[must_use]
pub fn handle_package_map_post_conditions(
    mut resolution: PackageMapResolution,
) -> PackageMapResolution {
    if !matches!(
        resolution.status,
        PackageMapStatus::Exact | PackageMapStatus::ExactEndsWithStar | PackageMapStatus::Inexact
    ) {
        return resolution;
    }
    let decoded = if let Ok(decoded) = decode_percent_escaped(resolution.path.as_bytes()) {
        String::from_utf8_lossy(&decoded).into_owned()
    } else {
        resolution.status = PackageMapStatus::InvalidModuleSpecifier;
        return resolution;
    };
    if ["%2f", "%2F", "%5c", "%5C"]
        .iter()
        .any(|encoding| resolution.path.contains(encoding))
    {
        resolution.status = PackageMapStatus::InvalidModuleSpecifier;
        return resolution;
    }
    if decoded.ends_with(['/', '\\']) {
        resolution.status = PackageMapStatus::UnsupportedDirectoryImport;
        return resolution;
    }
    resolution.path = decoded;
    resolution
}

#[must_use]
pub fn resolve_package_imports<S: BuildHasher>(
    specifier: &str,
    imports: &PackageMapEntry,
    conditions: &HashMap<String, bool, S>,
) -> PackageMapResolution {
    if imports.kind != PackageMapKind::Object {
        return package_resolution(
            "",
            PackageMapStatus::InvalidPackageConfiguration,
            imports.first_token,
        );
    }
    let resolution = resolve_package_imports_exports(specifier, imports, "/", true, conditions);
    if !matches!(
        resolution.status,
        PackageMapStatus::Null | PackageMapStatus::Undefined
    ) {
        return resolution;
    }
    package_resolution(
        specifier,
        PackageMapStatus::PackageImportNotDefined,
        imports.first_token,
    )
}

#[must_use]
pub fn resolve_package_exports<S: BuildHasher>(
    package_url: &str,
    subpath: &str,
    exports: &PackageMapEntry,
    conditions: &HashMap<String, bool, S>,
) -> PackageMapResolution {
    if exports.kind == PackageMapKind::Invalid {
        return package_resolution(
            "",
            PackageMapStatus::InvalidPackageConfiguration,
            exports.first_token,
        );
    }

    let mut debug = PackageMapDebug {
        token: exports.first_token,
        ..PackageMapDebug::default()
    };
    if subpath == "." {
        let main_export = if matches!(exports.kind, PackageMapKind::String | PackageMapKind::Array)
            || (exports.kind == PackageMapKind::Object && !exports.keys_start_with_dot())
        {
            Some(exports)
        } else if exports.kind == PackageMapKind::Object {
            exports.value_for_key(".")
        } else {
            None
        };
        if let Some(main_export) = main_export {
            let resolution =
                resolve_package_target(package_url, main_export, "", false, false, conditions);
            if !matches!(
                resolution.status,
                PackageMapStatus::Null | PackageMapStatus::Undefined
            ) {
                return resolution;
            }
            debug = resolution.debug;
        }
    } else if exports.kind == PackageMapKind::Object && exports.keys_start_with_dot() {
        let resolution =
            resolve_package_imports_exports(subpath, exports, package_url, false, conditions);
        if !matches!(
            resolution.status,
            PackageMapStatus::Null | PackageMapStatus::Undefined
        ) {
            return resolution;
        }
        debug = resolution.debug;
    }

    PackageMapResolution {
        status: PackageMapStatus::PackagePathNotExported,
        debug,
        ..PackageMapResolution::default()
    }
}

fn resolve_package_imports_exports<S: BuildHasher>(
    match_key: &str,
    match_object: &PackageMapEntry,
    package_url: &str,
    is_imports: bool,
    conditions: &HashMap<String, bool, S>,
) -> PackageMapResolution {
    if !match_key.ends_with('/')
        && !match_key.contains('*')
        && let Some(target) = match_object.value_for_key(match_key)
    {
        return resolve_package_target(package_url, target, "", false, is_imports, conditions);
    }

    for expansion in &match_object.expansion_keys {
        if let Some(star) = expansion.key.find('*') {
            let pattern_base = &expansion.key[..star];
            if match_key.starts_with(pattern_base) {
                let pattern_trailer = &expansion.key[star + 1..];
                if pattern_trailer.is_empty()
                    || (match_key.ends_with(pattern_trailer)
                        && match_key.len() >= expansion.key.len())
                {
                    let subpath =
                        &match_key[pattern_base.len()..match_key.len() - pattern_trailer.len()];
                    return resolve_package_target(
                        package_url,
                        &expansion.value,
                        subpath,
                        true,
                        is_imports,
                        conditions,
                    );
                }
            }
        } else if match_key.starts_with(&expansion.key) {
            let subpath = &match_key[expansion.key.len()..];
            let mut resolution = resolve_package_target(
                package_url,
                &expansion.value,
                subpath,
                false,
                is_imports,
                conditions,
            );
            if matches!(
                resolution.status,
                PackageMapStatus::Exact | PackageMapStatus::ExactEndsWithStar
            ) {
                resolution.status = PackageMapStatus::Inexact;
            }
            return resolution;
        }
    }
    package_resolution("", PackageMapStatus::Null, match_object.first_token)
}

#[allow(clippy::too_many_lines)]
fn resolve_package_target<S: BuildHasher>(
    package_url: &str,
    target: &PackageMapEntry,
    subpath: &str,
    pattern: bool,
    internal: bool,
    conditions: &HashMap<String, bool, S>,
) -> PackageMapResolution {
    match target.kind {
        PackageMapKind::String => {
            if !pattern && !subpath.is_empty() && !target.string.ends_with('/') {
                return PackageMapResolution {
                    path: target.string.clone(),
                    status: PackageMapStatus::InvalidModuleSpecifier,
                    debug: PackageMapDebug {
                        token: target.first_token,
                        invalid_because: " because it doesn't end in \"/\"".into(),
                        ..PackageMapDebug::default()
                    },
                };
            }
            if !target.string.starts_with("./") {
                if internal && !target.string.starts_with("../") && !target.string.starts_with('/')
                {
                    return package_resolution(
                        &if pattern {
                            target.string.replace('*', subpath)
                        } else {
                            format!("{}{subpath}", target.string)
                        },
                        PackageMapStatus::PackageResolve,
                        target.first_token,
                    );
                }
                return PackageMapResolution {
                    path: target.string.clone(),
                    status: PackageMapStatus::InvalidPackageTarget,
                    debug: PackageMapDebug {
                        token: target.first_token,
                        invalid_because: " because it doesn't start with \"./\"".into(),
                        ..PackageMapDebug::default()
                    },
                };
            }
            if let Some(segment) = find_invalid_package_segment(&target.string) {
                return PackageMapResolution {
                    path: target.string.clone(),
                    status: PackageMapStatus::InvalidPackageTarget,
                    debug: PackageMapDebug {
                        token: target.first_token,
                        invalid_because: format!(
                            " because it contains invalid segment {segment:?}"
                        ),
                        ..PackageMapDebug::default()
                    },
                };
            }
            let resolved_target = posix_path_join(package_url, &target.string);
            if let Some(segment) = find_invalid_package_segment(subpath) {
                return PackageMapResolution {
                    path: subpath.to_string(),
                    status: PackageMapStatus::InvalidModuleSpecifier,
                    debug: PackageMapDebug {
                        token: target.first_token,
                        invalid_because: format!(
                            " because it contains invalid segment {segment:?}"
                        ),
                        ..PackageMapDebug::default()
                    },
                };
            }
            if pattern {
                let status = if resolved_target.ends_with('*')
                    && resolved_target.find('*') == Some(resolved_target.len() - 1)
                {
                    PackageMapStatus::ExactEndsWithStar
                } else {
                    PackageMapStatus::Exact
                };
                package_resolution(
                    &resolved_target.replace('*', subpath),
                    status,
                    target.first_token,
                )
            } else {
                package_resolution(
                    &posix_path_join(&resolved_target, subpath),
                    PackageMapStatus::Exact,
                    target.first_token,
                )
            }
        }
        PackageMapKind::Object => {
            let mut matched_but_undefined = None;
            for property in &target.map {
                if property.key == "default"
                    || conditions.get(&property.key).copied().unwrap_or(false)
                {
                    let resolution = resolve_package_target(
                        package_url,
                        &property.value,
                        subpath,
                        pattern,
                        internal,
                        conditions,
                    );
                    if resolution.status.is_undefined() {
                        matched_but_undefined = Some(&property.value);
                        continue;
                    }
                    return resolution;
                }
            }
            let unmatched_target = matched_but_undefined
                .filter(|entry| {
                    entry.kind == PackageMapKind::Object && !entry.keys_start_with_dot()
                })
                .unwrap_or(target);
            if !unmatched_target.map.is_empty() && !unmatched_target.keys_start_with_dot() {
                return PackageMapResolution {
                    status: PackageMapStatus::UndefinedNoConditionsMatch,
                    debug: PackageMapDebug {
                        token: unmatched_target.first_token,
                        unmatched_conditions: unmatched_target
                            .map
                            .iter()
                            .map(|property| crate::internal::logger::Span {
                                text: property.key.clone(),
                                range: property.key_range,
                            })
                            .collect(),
                        ..PackageMapDebug::default()
                    },
                    ..PackageMapResolution::default()
                };
            }
            package_resolution("", PackageMapStatus::Undefined, target.first_token)
        }
        PackageMapKind::Array => {
            if target.array.is_empty() {
                return package_resolution("", PackageMapStatus::Null, target.first_token);
            }
            let mut last_status = PackageMapStatus::Undefined;
            let mut last_debug = PackageMapDebug {
                token: target.first_token,
                ..PackageMapDebug::default()
            };
            for item in &target.array {
                let resolution = resolve_package_target(
                    package_url,
                    item,
                    subpath,
                    pattern,
                    internal,
                    conditions,
                );
                if matches!(
                    resolution.status,
                    PackageMapStatus::InvalidPackageTarget | PackageMapStatus::Null
                ) {
                    last_status = resolution.status;
                    last_debug = resolution.debug;
                    continue;
                }
                if resolution.status.is_undefined() {
                    continue;
                }
                return resolution;
            }
            PackageMapResolution {
                status: last_status,
                debug: last_debug,
                ..PackageMapResolution::default()
            }
        }
        PackageMapKind::Null => PackageMapResolution {
            status: PackageMapStatus::Null,
            debug: PackageMapDebug {
                token: target.first_token,
                is_because_of_null_literal: true,
                ..PackageMapDebug::default()
            },
            ..PackageMapResolution::default()
        },
        PackageMapKind::Invalid => package_resolution(
            "",
            PackageMapStatus::InvalidPackageTarget,
            target.first_token,
        ),
    }
}

fn package_resolution(path: &str, status: PackageMapStatus, token: Range) -> PackageMapResolution {
    PackageMapResolution {
        path: path.to_string(),
        status,
        debug: PackageMapDebug {
            token,
            ..PackageMapDebug::default()
        },
    }
}

fn posix_path_join(left: &str, right: &str) -> String {
    let joined = if right.is_empty() {
        left.to_string()
    } else if left.is_empty() {
        right.to_string()
    } else {
        format!("{left}/{right}")
    };
    let absolute = joined.starts_with('/');
    let mut segments = Vec::new();
    for segment in joined.split('/') {
        match segment {
            ".." if segments.last().is_some_and(|segment| *segment != "..") => {
                segments.pop();
            }
            ".." if !absolute => segments.push(segment),
            "" | "." | ".." => {}
            _ => segments.push(segment),
        }
    }
    let result = segments.join("/");
    if absolute {
        format!("/{result}")
    } else if result.is_empty() {
        ".".into()
    } else {
        result
    }
}

#[must_use]
pub fn reverse_resolve_package_exports<S: BuildHasher>(
    query: &str,
    root: &PackageMapEntry,
    conditions: &HashMap<String, bool, S>,
) -> Option<(String, Range)> {
    if root.kind == PackageMapKind::Object && root.keys_start_with_dot() {
        reverse_resolve_package_map(query, root, conditions)
    } else {
        None
    }
}

fn reverse_resolve_package_map<S: BuildHasher>(
    query: &str,
    map: &PackageMapEntry,
    conditions: &HashMap<String, bool, S>,
) -> Option<(String, Range)> {
    if !query.ends_with('*') {
        for property in &map.map {
            if let Some(result) = reverse_resolve_package_target(
                query,
                &property.key,
                &property.value,
                PackageReverseKind::Exact,
                conditions,
            ) {
                return Some(result);
            }
        }
    }
    for expansion in &map.expansion_keys {
        if expansion.key.ends_with('*')
            && let Some(result) = reverse_resolve_package_target(
                query,
                &expansion.key,
                &expansion.value,
                PackageReverseKind::Pattern,
                conditions,
            )
        {
            return Some(result);
        }
        if let Some(result) = reverse_resolve_package_target(
            query,
            &expansion.key,
            &expansion.value,
            PackageReverseKind::Prefix,
            conditions,
        ) {
            return Some(result);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum PackageReverseKind {
    Exact,
    Pattern,
    Prefix,
}

fn reverse_resolve_package_target<S: BuildHasher>(
    query: &str,
    key: &str,
    target: &PackageMapEntry,
    kind: PackageReverseKind,
    conditions: &HashMap<String, bool, S>,
) -> Option<(String, Range)> {
    match target.kind {
        PackageMapKind::String => match kind {
            PackageReverseKind::Exact if query == target.string => {
                Some((key.to_string(), target.first_token))
            }
            PackageReverseKind::Prefix if query.starts_with(&target.string) => Some((
                format!("{key}{}", &query[target.string.len()..]),
                target.first_token,
            )),
            PackageReverseKind::Pattern => {
                let key_without_star = key.strip_suffix('*').unwrap_or(key);
                let Some(star) = target.string.find('*') else {
                    return (query == target.string)
                        .then(|| (key_without_star.to_string(), target.first_token));
                };
                let prefix = &target.string[..star];
                let suffix = &target.string[star + 1..];
                if suffix.contains('*') || !query.starts_with(prefix) {
                    return None;
                }
                let after_prefix = &query[prefix.len()..];
                after_prefix
                    .strip_suffix(suffix)
                    .map(|matched| (format!("{key_without_star}{matched}"), target.first_token))
            }
            _ => None,
        },
        PackageMapKind::Object => target.map.iter().find_map(|property| {
            (property.key == "default" || conditions.get(&property.key).copied().unwrap_or(false))
                .then(|| {
                    reverse_resolve_package_target(query, key, &property.value, kind, conditions)
                })
                .flatten()
        }),
        PackageMapKind::Array => target
            .array
            .iter()
            .find_map(|item| reverse_resolve_package_target(query, key, item, kind, conditions)),
        PackageMapKind::Null | PackageMapKind::Invalid => None,
    }
}

pub struct PnpData {
    fallback_exclusion_list: HashMap<String, HashMap<String, bool>>,
    fallback_pool: HashMap<String, PnpIdentAndReference>,
    ignore_pattern_data: Option<regex::Regex>,
    pub invalid_ignore_pattern_data: String,
    package_registry_data: HashMap<String, HashMap<String, PnpPackage>>,
    package_locators_by_locations: HashMap<String, PnpPackageLocatorByLocation>,
    enable_top_level_fallback: bool,
    pub abs_path: String,
    pub abs_dir_path: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PnpIdentAndReference {
    ident: String,
    reference: String,
    span: Range,
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_field_names)]
struct PnpPackage {
    package_dependencies: HashMap<String, PnpIdentAndReference>,
    package_location: String,
    package_dependencies_range: Range,
}

#[derive(Clone, Debug, Default)]
struct PnpPackageLocatorByLocation {
    locator: PnpIdentAndReference,
    discard_from_lookup: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PnpStatus {
    #[default]
    ErrorGeneric,
    ErrorDependencyNotFound,
    ErrorUnfulfilledPeerDependency,
    Success,
    Skipped,
}

impl PnpStatus {
    #[must_use]
    pub const fn is_error(self) -> bool {
        matches!(
            self,
            Self::ErrorGeneric
                | Self::ErrorDependencyNotFound
                | Self::ErrorUnfulfilledPeerDependency
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PnpResult {
    pub status: PnpStatus,
    pub package_dir_path: String,
    pub package_ident: String,
    pub package_subpath: String,
    pub error_ident: String,
    pub error_range: Range,
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn compile_yarn_pnp_data(abs_path: &str, abs_dir_path: &str, json: &Expr) -> PnpData {
    let mut data = PnpData {
        fallback_exclusion_list: HashMap::new(),
        fallback_pool: HashMap::new(),
        ignore_pattern_data: None,
        invalid_ignore_pattern_data: String::new(),
        package_registry_data: HashMap::new(),
        package_locators_by_locations: HashMap::new(),
        enable_top_level_fallback: false,
        abs_path: abs_path.to_string(),
        abs_dir_path: abs_dir_path.to_string(),
    };

    if let Some((value, _)) = get_property(json, "enableTopLevelFallback")
        && let Some(enabled) = get_bool(value)
    {
        data.enable_top_level_fallback = enabled;
    }
    if let Some((value, _)) = get_property(json, "fallbackExclusionList")
        && let Some(ExprData::Array(entries)) = value.data.as_deref()
    {
        for entry in &entries.items {
            let Some(ExprData::Array(tuple)) = entry.data.as_deref() else {
                continue;
            };
            if tuple.items.len() != 2 {
                continue;
            }
            let Some(ident) = get_string_or_null(&tuple.items[0]) else {
                continue;
            };
            let Some(ExprData::Array(references)) = tuple.items[1].data.as_deref() else {
                continue;
            };
            data.fallback_exclusion_list.insert(
                ident,
                references
                    .items
                    .iter()
                    .filter_map(get_string)
                    .map(|reference| (reference, true))
                    .collect(),
            );
        }
    }
    if let Some((value, _)) = get_property(json, "fallbackPool")
        && let Some(ExprData::Array(entries)) = value.data.as_deref()
    {
        for entry in &entries.items {
            let Some(ExprData::Array(tuple)) = entry.data.as_deref() else {
                continue;
            };
            if tuple.items.len() == 2
                && let Some(ident) = get_string(&tuple.items[0])
                && let Some(target) = get_pnp_dependency_target(&tuple.items[1])
            {
                data.fallback_pool.insert(ident, target);
            }
        }
    }
    if let Some((value, _)) = get_property(json, "ignorePatternData")
        && let Some(mut pattern) = get_string(value)
    {
        for unsupported in [
            r"(?!\.)",
            r"(?!(?:^|\/)\.)",
            r"(?!\.{1,2}(?:\/|$))",
            r"(?!(?:^|\/)\.{1,2}(?:\/|$))",
        ] {
            pattern = pattern.replace(unsupported, "");
        }
        match regex::Regex::new(&pattern) {
            Ok(regex) => data.ignore_pattern_data = Some(regex),
            Err(_) => data.invalid_ignore_pattern_data = pattern,
        }
    }
    if let Some((value, _)) = get_property(json, "packageRegistryData")
        && let Some(ExprData::Array(idents)) = value.data.as_deref()
    {
        for ident_entry in &idents.items {
            let Some(ExprData::Array(ident_tuple)) = ident_entry.data.as_deref() else {
                continue;
            };
            if ident_tuple.items.len() != 2 {
                continue;
            }
            let Some(package_ident) = get_string_or_null(&ident_tuple.items[0]) else {
                continue;
            };
            let Some(ExprData::Array(references)) = ident_tuple.items[1].data.as_deref() else {
                continue;
            };
            let mut packages = HashMap::new();
            for reference_entry in &references.items {
                let Some(ExprData::Array(reference_tuple)) = reference_entry.data.as_deref() else {
                    continue;
                };
                if reference_tuple.items.len() != 2 {
                    continue;
                }
                let Some(package_reference) = get_string_or_null(&reference_tuple.items[0]) else {
                    continue;
                };
                let package = &reference_tuple.items[1];
                let Some((location_value, _)) = get_property(package, "packageLocation") else {
                    continue;
                };
                let Some(package_location) = get_string(location_value) else {
                    continue;
                };
                let Some((dependencies_value, _)) = get_property(package, "packageDependencies")
                else {
                    continue;
                };
                let Some(ExprData::Array(dependencies)) = dependencies_value.data.as_deref() else {
                    continue;
                };
                let mut package_dependencies = HashMap::new();
                for dependency in &dependencies.items {
                    let Some(ExprData::Array(tuple)) = dependency.data.as_deref() else {
                        continue;
                    };
                    if tuple.items.len() == 2
                        && let Some(ident) = get_string(&tuple.items[0])
                        && let Some(target) = get_pnp_dependency_target(&tuple.items[1])
                    {
                        package_dependencies.insert(ident, target);
                    }
                }
                let discard_from_lookup = get_property(package, "discardFromLookup")
                    .and_then(|(value, _)| get_bool(value))
                    .unwrap_or(false);
                packages.insert(
                    package_reference.clone(),
                    PnpPackage {
                        package_dependencies,
                        package_location: package_location.clone(),
                        package_dependencies_range: Range {
                            loc: dependencies_value.loc,
                            len: dependencies.close_bracket_loc.start + 1
                                - dependencies_value.loc.start,
                        },
                    },
                );
                data.package_locators_by_locations
                    .entry(package_location)
                    .and_modify(|entry| {
                        entry.discard_from_lookup &= discard_from_lookup;
                        if !discard_from_lookup {
                            entry.locator = PnpIdentAndReference {
                                ident: package_ident.clone(),
                                reference: package_reference.clone(),
                                ..PnpIdentAndReference::default()
                            };
                        }
                    })
                    .or_insert_with(|| PnpPackageLocatorByLocation {
                        locator: PnpIdentAndReference {
                            ident: package_ident.clone(),
                            reference: package_reference,
                            ..PnpIdentAndReference::default()
                        },
                        discard_from_lookup,
                    });
            }
            data.package_registry_data.insert(package_ident, packages);
        }
    }
    data
}

impl PnpData {
    #[must_use]
    pub fn resolve_to_unqualified(
        &self,
        specifier: &str,
        parent_url: &str,
        file_system: &dyn Fs,
    ) -> PnpResult {
        let Some((ident, module_path)) = parse_bare_identifier(specifier) else {
            return PnpResult::default();
        };
        let Some(parent_locator) = self.find_locator(parent_url, file_system) else {
            return PnpResult {
                status: PnpStatus::Skipped,
                ..PnpResult::default()
            };
        };
        let Some(parent_package) =
            self.get_package(&parent_locator.ident, &parent_locator.reference)
        else {
            return PnpResult::default();
        };
        let mut target = parent_package.package_dependencies.get(ident).cloned();
        if target
            .as_ref()
            .is_none_or(|target| target.reference.is_empty())
            && self.enable_top_level_fallback
            && !self
                .fallback_exclusion_list
                .get(&parent_locator.ident)
                .is_some_and(|references| {
                    references
                        .get(&parent_locator.reference)
                        .copied()
                        .unwrap_or(false)
                })
        {
            target = self.resolve_via_fallback(ident);
        }
        let Some(target) = target else {
            return PnpResult {
                status: PnpStatus::ErrorDependencyNotFound,
                error_ident: ident.to_string(),
                error_range: parent_package.package_dependencies_range,
                ..PnpResult::default()
            };
        };
        if target.reference.is_empty() {
            return PnpResult {
                status: PnpStatus::ErrorUnfulfilledPeerDependency,
                error_ident: ident.to_string(),
                error_range: target.span,
                ..PnpResult::default()
            };
        }
        let dependency_package = if target.ident.is_empty() {
            self.get_package(ident, &target.reference)
        } else {
            self.get_package(&target.ident, &target.reference)
        };
        let Some(dependency_package) = dependency_package else {
            return PnpResult::default();
        };
        let mut base = self.abs_dir_path.clone();
        let windows = !base.starts_with('/');
        if windows {
            base = format!("/{}", base.replace('\\', "/"));
        }
        let mut package_dir_path = posix_path_join(&base, &dependency_package.package_location);
        if windows {
            package_dir_path = package_dir_path
                .strip_prefix('/')
                .unwrap_or(&package_dir_path)
                .to_string();
        }
        PnpResult {
            status: PnpStatus::Success,
            package_dir_path,
            package_ident: ident.to_string(),
            package_subpath: module_path.to_string(),
            ..PnpResult::default()
        }
    }

    fn find_locator(&self, module_url: &str, file_system: &dyn Fs) -> Option<PnpIdentAndReference> {
        let mut relative = file_system.rel(&self.abs_dir_path, module_url)?;
        relative = relative.replace('\\', "/");
        relative = relative.strip_prefix("./").unwrap_or(&relative).to_string();
        if self
            .ignore_pattern_data
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(&relative))
        {
            return None;
        }
        if !relative.ends_with('/') {
            relative.push('/');
        }
        if !relative.starts_with("./") && !relative.starts_with("../") {
            relative = format!("./{relative}");
        }
        loop {
            if let Some(entry) = self.package_locators_by_locations.get(&relative)
                && !entry.discard_from_lookup
            {
                return Some(entry.locator.clone());
            }
            let without_slash = relative.strip_suffix('/').unwrap_or(&relative);
            let last_slash = without_slash.rfind('/')?;
            relative.truncate(last_slash + 1);
            if relative.is_empty() {
                return None;
            }
        }
    }

    fn resolve_via_fallback(&self, ident: &str) -> Option<PnpIdentAndReference> {
        self.get_package("", "")
            .and_then(|package| package.package_dependencies.get(ident))
            .cloned()
            .or_else(|| self.fallback_pool.get(ident).cloned())
    }

    fn get_package(&self, ident: &str, reference: &str) -> Option<&PnpPackage> {
        self.package_registry_data.get(ident)?.get(reference)
    }
}

fn get_string_or_null(expression: &Expr) -> Option<String> {
    match expression.data.as_deref()? {
        ExprData::Null => Some(String::new()),
        ExprData::String(_) => get_string(expression),
        _ => None,
    }
}

fn get_pnp_dependency_target(expression: &Expr) -> Option<PnpIdentAndReference> {
    match expression.data.as_deref()? {
        ExprData::Null => Some(PnpIdentAndReference {
            span: Range {
                loc: expression.loc,
                len: 4,
            },
            ..PnpIdentAndReference::default()
        }),
        ExprData::String(_) => Some(PnpIdentAndReference {
            reference: get_string(expression)?,
            span: Range {
                loc: expression.loc,
                ..Range::default()
            },
            ..PnpIdentAndReference::default()
        }),
        ExprData::Array(array) if array.items.len() == 2 => Some(PnpIdentAndReference {
            ident: get_string(&array.items[0])?,
            reference: get_string(&array.items[1])?,
            span: Range {
                loc: expression.loc,
                len: array.close_bracket_loc.start + 1 - expression.loc.start,
            },
        }),
        _ => None,
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
        DataUrl, DebugMeta, MimeType, PackageMapKind, PackageMapStatus, PathPair, PnpStatus,
        ResolverContext, TsConfigJson, TsConfigPath, TsConfigPaths, compile_yarn_pnp_data,
        find_invalid_package_segment, globstar_to_escaped_regexp,
        handle_package_map_post_conditions, is_node_builtin, is_package_path,
        is_valid_tsconfig_path_no_base_url_pattern, load_as_directory, load_as_file, load_as_index,
        match_tsconfig_path_candidates, parse_bare_identifier, parse_esm_package_name,
        parse_imports_exports_map, parse_package_json, parse_tsconfig_json,
        resolve_file_or_package, resolve_file_or_package_with_context, resolve_package_exports,
        resolve_package_imports, resolve_with_metadata, reverse_resolve_package_exports,
        sort_package_expansion_keys,
    };
    use crate::internal::{
        config::{MaybeBool, Platform, TsJsx, TsTarget},
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

    #[test]
    fn resolves_package_exports_exact_patterns_and_prefixes() {
        let contents =
            r#"{"./features/*":"./src/*.js","./legacy/":"./old/","./bad":"../escape.js"}"#;
        let source = Source {
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
        assert!(ok);
        let map = parse_imports_exports_map(&source, &log, &json, "exports", Loc::default())
            .expect("package map");
        let conditions = HashMap::new();

        let pattern = resolve_package_exports("/pkg", "./features/button", &map.root, &conditions);
        assert_eq!(pattern.status, PackageMapStatus::Exact);
        assert_eq!(pattern.path, "/pkg/src/button.js");

        let prefix = resolve_package_exports("/pkg", "./legacy/file", &map.root, &conditions);
        assert_eq!(prefix.status, PackageMapStatus::Inexact);
        assert_eq!(prefix.path, "/pkg/old/file");

        assert_eq!(
            resolve_package_exports("/pkg", "./missing", &map.root, &conditions).status,
            PackageMapStatus::PackagePathNotExported
        );
        let invalid = resolve_package_exports("/pkg", "./bad", &map.root, &conditions);
        assert_eq!(invalid.status, PackageMapStatus::InvalidPackageTarget);
        assert_eq!(
            invalid.debug.invalid_because,
            " because it doesn't start with \"./\""
        );
    }

    #[test]
    fn resolves_package_conditions_arrays_and_internal_imports() {
        let source = Source {
            contents: Arc::from(&br#"{"import":"./esm.js","require":"./cjs.js"}"#[..]),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
        assert!(ok);
        let exports = parse_imports_exports_map(&source, &log, &json, "exports", Loc::default())
            .expect("exports map");
        let import_conditions = HashMap::from([("import".into(), true)]);
        let resolved = resolve_package_exports("/pkg", ".", &exports.root, &import_conditions);
        assert_eq!(resolved.status, PackageMapStatus::Exact);
        assert_eq!(resolved.path, "/pkg/esm.js");

        let unmatched = resolve_package_exports("/pkg", ".", &exports.root, &HashMap::new());
        assert_eq!(
            unmatched.status,
            PackageMapStatus::UndefinedNoConditionsMatch
        );
        assert_eq!(
            unmatched
                .debug
                .unmatched_conditions
                .iter()
                .map(|condition| condition.text.as_str())
                .collect::<Vec<_>>(),
            vec!["import", "require"]
        );

        let source = Source {
            contents: Arc::from(&br##"{"#dep":["../bad","package/subpath"]}"##[..]),
            ..Source::default()
        };
        let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
        assert!(ok);
        let imports = parse_imports_exports_map(&source, &log, &json, "imports", Loc::default())
            .expect("imports map");
        let resolved = resolve_package_imports("#dep", &imports.root, &HashMap::new());
        assert_eq!(resolved.status, PackageMapStatus::PackageResolve);
        assert_eq!(resolved.path, "package/subpath");
    }

    #[test]
    fn validates_package_map_post_conditions() {
        use super::{PackageMapResolution, PackageMapStatus};

        let decoded = handle_package_map_post_conditions(PackageMapResolution {
            path: "/pkg/hello%20world.js".into(),
            status: PackageMapStatus::Exact,
            ..PackageMapResolution::default()
        });
        assert_eq!(decoded.path, "/pkg/hello world.js");
        assert_eq!(decoded.status, PackageMapStatus::Exact);

        for (path, status) in [
            ("/pkg/a%2Fb.js", PackageMapStatus::InvalidModuleSpecifier),
            ("/pkg/bad%xx", PackageMapStatus::InvalidModuleSpecifier),
            (
                "/pkg/directory/",
                PackageMapStatus::UnsupportedDirectoryImport,
            ),
        ] {
            assert_eq!(
                handle_package_map_post_conditions(PackageMapResolution {
                    path: path.into(),
                    status: PackageMapStatus::Exact,
                    ..PackageMapResolution::default()
                })
                .status,
                status
            );
        }
    }

    #[test]
    fn reverse_resolves_public_package_subpaths() {
        let contents = r#"{
          "./exact": "./dist/exact.js",
          "./features/*": "./src/*.js",
          "./legacy/": "./old/",
          "./conditional": {
            "browser": "./browser.js",
            "default": "./default.js"
          }
        }"#;
        let source = Source {
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (json, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
        assert!(ok);
        let exports = parse_imports_exports_map(&source, &log, &json, "exports", Loc::default())
            .expect("exports map");

        assert_eq!(
            reverse_resolve_package_exports("./dist/exact.js", &exports.root, &HashMap::new())
                .map(|result| result.0),
            Some("./exact".into())
        );
        assert_eq!(
            reverse_resolve_package_exports("./src/button.js", &exports.root, &HashMap::new())
                .map(|result| result.0),
            Some("./features/button".into())
        );
        assert_eq!(
            reverse_resolve_package_exports("./old/file.js", &exports.root, &HashMap::new())
                .map(|result| result.0),
            Some("./legacy/file.js".into())
        );
        assert_eq!(
            reverse_resolve_package_exports(
                "./browser.js",
                &exports.root,
                &HashMap::from([("browser".into(), true)])
            )
            .map(|result| result.0),
            Some("./conditional".into())
        );
        assert!(
            reverse_resolve_package_exports("./private.js", &exports.root, &HashMap::new())
                .is_none()
        );
    }

    #[test]
    fn compiles_and_resolves_yarn_pnp_manifests() {
        let contents = r#"{
          "enableTopLevelFallback": true,
          "fallbackPool": [["fallback", "npm:1"]],
          "packageRegistryData": [
            [null, [[null, {
              "packageLocation": "./",
              "packageDependencies": [
                ["foo", "npm:1"],
                ["alias", ["foo", "npm:1"]],
                ["peer", null]
              ]
            }]]],
            ["foo", [["npm:1", {
              "packageLocation": "./.yarn/foo/",
              "packageDependencies": []
            }]]],
            ["fallback", [["npm:1", {
              "packageLocation": "./.yarn/fallback/",
              "packageDependencies": []
            }]]]
          ]
        }"#;
        let source = Source {
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (json, ok) = parse_json(log, source, JsonOptions::default());
        assert!(ok);
        let manifest = compile_yarn_pnp_data("/project/.pnp.data.json", "/project/", &json);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");

        let foo =
            manifest.resolve_to_unqualified("foo/subpath", "/project/src/index.js", &file_system);
        assert_eq!(foo.status, PnpStatus::Success);
        assert_eq!(foo.package_dir_path, "/project/.yarn/foo");
        assert_eq!(foo.package_subpath, "/subpath");

        let alias = manifest.resolve_to_unqualified("alias", "/project/index.js", &file_system);
        assert_eq!(alias.status, PnpStatus::Success);
        assert_eq!(alias.package_dir_path, "/project/.yarn/foo");
        assert_eq!(alias.package_ident, "alias");

        assert_eq!(
            manifest
                .resolve_to_unqualified("peer", "/project/index.js", &file_system)
                .status,
            PnpStatus::ErrorUnfulfilledPeerDependency
        );
        let fallback =
            manifest.resolve_to_unqualified("fallback", "/project/index.js", &file_system);
        assert_eq!(fallback.status, PnpStatus::Success);
        assert_eq!(fallback.package_dir_path, "/project/.yarn/fallback");
        assert_eq!(
            manifest
                .resolve_to_unqualified("foo", "/outside/index.js", &file_system)
                .status,
            PnpStatus::Skipped
        );
    }

    #[test]
    fn parses_package_json_resolution_metadata() {
        let contents = r##"{
          "name": "demo",
          "type": "module",
          "main": "./index.cjs",
          "module": "./index.js",
          "browser": {"fs": false, "./index.js": "./browser.js"},
          "sideEffects": ["styles.css", "./polyfill.js"],
          "imports": {"#internal": "./internal.js"},
          "exports": {".": "./index.js"}
        }"##;
        let source = Source {
            key_path: Path {
                text: "/project/package.json".into(),
                ..Path::default()
            },
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let package = parse_package_json(
            &log,
            &source,
            "/project",
            &file_system,
            Platform::Browser,
            None,
        )
        .expect("package json");
        assert_eq!(package.name, "demo");
        assert_eq!(
            package.module_type_data.module_type,
            crate::internal::js_ast::ModuleType::EsmPackageJson
        );
        assert_eq!(
            package
                .main_fields
                .get("module")
                .expect("module field")
                .relative_path,
            "./index.js"
        );
        assert_eq!(package.browser_map.get("fs"), Some(&None));
        assert_eq!(
            package.browser_map.get("./index.js"),
            Some(&Some("./browser.js".into()))
        );
        assert_eq!(package.side_effects_regexps.len(), 1);
        assert_eq!(
            package
                .side_effects_map
                .as_ref()
                .expect("side effects")
                .get("/project/polyfill.js"),
            Some(&true)
        );
        assert!(package.imports_map.is_some());
        assert!(package.exports_map.is_some());
        assert!(log.done().is_empty());
    }

    #[test]
    fn loads_files_extensions_rewrites_and_directory_indexes() {
        let file_system = mock_fs(
            &HashMap::from([
                ("/project/exact.js".into(), String::new()),
                ("/project/component.js.ts".into(), String::new()),
                ("/project/component.ts".into(), String::new()),
                ("/project/module.mts".into(), String::new()),
                ("/project/pkg/index.ts".into(), String::new()),
                ("/project/pkg/index.js".into(), String::new()),
                ("/project/Case.JS".into(), String::new()),
            ]),
            MockKind::Unix,
            "/",
        );
        let extensions = vec![".js".into(), ".ts".into()];

        assert_eq!(
            load_as_file(&file_system, "/project/exact.js", &extensions)
                .expect("exact file")
                .path,
            "/project/exact.js"
        );
        assert_eq!(
            load_as_file(&file_system, "/project/component.js", &extensions)
                .expect("extension before rewrite")
                .path,
            "/project/component.js.ts"
        );
        assert_eq!(
            load_as_file(&file_system, "/project/module.mjs", &extensions)
                .expect("TypeScript rewrite")
                .path,
            "/project/module.mts"
        );
        assert_eq!(
            load_as_index(&file_system, "/project/pkg", &extensions)
                .expect("ordered index")
                .path,
            "/project/pkg/index.js"
        );
        let different_case = load_as_file(&file_system, "/project/case.js", &extensions)
            .expect("case-insensitive mock lookup")
            .different_case
            .expect("different case");
        assert_eq!(different_case.actual, "Case.JS");
        assert!(load_as_file(&file_system, "/project/missing", &extensions).is_none());
    }

    #[test]
    fn loads_directory_main_module_browser_and_require_fallbacks() {
        let package_json = r#"{
          "main": "./index.cjs",
          "module": "./index.js",
          "browser": {
            "./index.js": "./browser.js",
            "./index.cjs": false
          }
        }"#;
        let file_system = mock_fs(
            &HashMap::from([
                ("/project/pkg/package.json".into(), package_json.into()),
                ("/project/pkg/index.cjs".into(), String::new()),
                ("/project/pkg/index.js".into(), String::new()),
                ("/project/pkg/browser.js".into(), String::new()),
                ("/project/fallback/index.ts".into(), String::new()),
            ]),
            MockKind::Unix,
            "/",
        );
        let extensions = vec![".js".into(), ".cjs".into(), ".ts".into()];
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());

        let imported = load_as_directory(
            &log,
            &file_system,
            "/project/pkg",
            &extensions,
            Platform::Browser,
            None,
            false,
        )
        .expect("imported package");
        assert_eq!(imported.paths.primary.text, "/project/pkg/browser.js");
        assert_eq!(imported.paths.secondary.text, "/project/pkg/index.cjs");
        assert!(imported.paths.secondary.is_disabled());

        let required = load_as_directory(
            &log,
            &file_system,
            "/project/pkg",
            &extensions,
            Platform::Browser,
            None,
            true,
        )
        .expect("required package");
        assert_eq!(required.paths.primary.text, "/project/pkg/index.cjs");
        assert!(required.paths.primary.is_disabled());

        let configured = vec!["main".into()];
        let disabled = load_as_directory(
            &log,
            &file_system,
            "/project/pkg",
            &extensions,
            Platform::Browser,
            Some(&configured),
            false,
        )
        .expect("disabled browser main");
        assert!(disabled.paths.primary.is_disabled());

        let fallback = load_as_directory(
            &log,
            &file_system,
            "/project/fallback",
            &extensions,
            Platform::Neutral,
            None,
            false,
        )
        .expect("index fallback");
        assert_eq!(fallback.paths.primary.text, "/project/fallback/index.ts");
    }

    #[test]
    fn resolves_relative_and_ancestor_node_modules_paths() {
        let package_json = r#"{
          "exports": {
            ".": "./main.js",
            "./feature/*": "./src/*.js",
            "./no-extension": "./src/no-extension"
          }
        }"#;
        let file_system = mock_fs(
            &HashMap::from([
                ("/project/src/local.ts".into(), String::new()),
                (
                    "/project/node_modules/pkg/package.json".into(),
                    package_json.into(),
                ),
                ("/project/node_modules/pkg/main.js".into(), String::new()),
                (
                    "/project/node_modules/pkg/src/button.js".into(),
                    String::new(),
                ),
                (
                    "/project/node_modules/pkg/src/no-extension.js".into(),
                    String::new(),
                ),
                (
                    "/project/src/node_modules/nearest/index.js".into(),
                    String::new(),
                ),
                (
                    "/project/node_modules/nearest/index.js".into(),
                    String::new(),
                ),
            ]),
            MockKind::Unix,
            "/",
        );
        let extensions = vec![".js".into(), ".ts".into()];
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());

        assert!(!is_package_path("./local"));
        assert!(is_package_path("pkg/feature/button"));
        assert_eq!(
            resolve_file_or_package(
                &log,
                &file_system,
                "/project/src/deep",
                "../local",
                &extensions,
                Platform::Browser,
                None,
                false,
            )
            .expect("relative import")
            .paths
            .primary
            .text,
            "/project/src/local.ts"
        );
        assert_eq!(
            resolve_file_or_package(
                &log,
                &file_system,
                "/project/src/deep",
                "pkg",
                &extensions,
                Platform::Browser,
                None,
                false,
            )
            .expect("package root export")
            .paths
            .primary
            .text,
            "/project/node_modules/pkg/main.js"
        );
        assert_eq!(
            resolve_file_or_package(
                &log,
                &file_system,
                "/project/src/deep",
                "pkg/feature/button",
                &extensions,
                Platform::Browser,
                None,
                false,
            )
            .expect("package wildcard export")
            .paths
            .primary
            .text,
            "/project/node_modules/pkg/src/button.js"
        );
        assert!(
            resolve_file_or_package(
                &log,
                &file_system,
                "/project/src/deep",
                "pkg/no-extension",
                &extensions,
                Platform::Browser,
                None,
                false,
            )
            .is_none(),
            "exact exports targets must not gain implicit extensions"
        );
        assert_eq!(
            resolve_file_or_package(
                &log,
                &file_system,
                "/project/src/deep",
                "nearest",
                &extensions,
                Platform::Browser,
                None,
                false,
            )
            .expect("nearest node_modules")
            .paths
            .primary
            .text,
            "/project/src/node_modules/nearest/index.js"
        );
    }

    #[test]
    fn resolver_context_applies_tsconfig_and_package_imports() {
        let file_system = mock_fs(
            &HashMap::from([
                ("/project/generated/alias.ts".into(), String::new()),
                (
                    "/project/package.json".into(),
                    r##"{"imports":{"#internal":"./internal.js"}}"##.into(),
                ),
                ("/project/internal.js".into(), String::new()),
            ]),
            MockKind::Unix,
            "/",
        );
        let extensions = vec![".js".into(), ".ts".into()];
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let tsconfig = TsConfigJson {
            base_url_for_paths: "/project".into(),
            paths: Some(TsConfigPaths {
                map: HashMap::from([(
                    "@app/*".into(),
                    vec![TsConfigPath {
                        text: "generated/*".into(),
                        ..TsConfigPath::default()
                    }],
                )]),
                ..TsConfigPaths::default()
            }),
            ..TsConfigJson::default()
        };
        let context = ResolverContext {
            tsconfig: Some(&tsconfig),
            ..ResolverContext::default()
        };

        assert_eq!(
            resolve_file_or_package_with_context(
                &log,
                &file_system,
                "/project/src",
                "@app/alias",
                &extensions,
                Platform::Browser,
                None,
                false,
                context,
            )
            .expect("tsconfig path")
            .paths
            .primary
            .text,
            "/project/generated/alias.ts"
        );
        assert_eq!(
            resolve_file_or_package_with_context(
                &log,
                &file_system,
                "/project/src",
                "#internal",
                &extensions,
                Platform::Browser,
                None,
                false,
                context,
            )
            .expect("package import")
            .paths
            .primary
            .text,
            "/project/internal.js"
        );
    }

    #[test]
    fn externalizes_node_builtins_and_strips_unsupported_prefixes() {
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let extensions = vec![".js".into()];
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        assert!(is_node_builtin("fs/promises"));
        assert!(!is_node_builtin("not-a-node-module"));

        let builtin = resolve_file_or_package(
            &log,
            &file_system,
            "/project",
            "fs",
            &extensions,
            Platform::Node,
            None,
            false,
        )
        .expect("node builtin");
        assert!(builtin.paths.is_external);
        assert_eq!(builtin.paths.primary.text, "fs");

        let prefixed = resolve_file_or_package_with_context(
            &log,
            &file_system,
            "/project",
            "node:custom",
            &extensions,
            Platform::Node,
            None,
            true,
            ResolverContext {
                strip_node_prefix_for_require: true,
                ..ResolverContext::default()
            },
        )
        .expect("node prefix");
        assert!(prefixed.paths.is_external);
        assert_eq!(prefixed.paths.primary.text, "custom");

        assert!(
            resolve_file_or_package(
                &log,
                &file_system,
                "/project",
                "fs",
                &extensions,
                Platform::Browser,
                None,
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn resolved_files_inherit_package_and_tsconfig_metadata() {
        let file_system = mock_fs(
            &HashMap::from([
                (
                    "/project/package.json".into(),
                    r#"{"type":"module","sideEffects":["keep.js"]}"#.into(),
                ),
                ("/project/keep.js".into(), String::new()),
                ("/project/drop.js".into(), String::new()),
            ]),
            MockKind::Unix,
            "/",
        );
        let extensions = vec![".js".into()];
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let tsconfig = TsConfigJson {
            settings: crate::internal::config::TsConfig {
                experimental_decorators: MaybeBool::True,
                ..crate::internal::config::TsConfig::default()
            },
            ts_always_strict: Some(crate::internal::config::TsAlwaysStrict {
                value: true,
                ..crate::internal::config::TsAlwaysStrict::default()
            }),
            ..TsConfigJson::default()
        };
        let context = ResolverContext {
            tsconfig: Some(&tsconfig),
            ..ResolverContext::default()
        };

        let keep = resolve_with_metadata(
            &log,
            &file_system,
            "/project",
            "./keep",
            &extensions,
            Platform::Browser,
            None,
            false,
            context,
        )
        .expect("kept file");
        assert_eq!(
            keep.module_type_data.module_type,
            crate::internal::js_ast::ModuleType::EsmPackageJson
        );
        assert!(keep.primary_side_effects_data.is_none());
        assert_eq!(
            keep.ts_config.expect("tsconfig").experimental_decorators,
            MaybeBool::True
        );
        assert!(keep.ts_always_strict.expect("always strict").value);

        let drop = resolve_with_metadata(
            &log,
            &file_system,
            "/project",
            "./drop",
            &extensions,
            Platform::Browser,
            None,
            false,
            context,
        )
        .expect("tree-shakeable file");
        assert!(
            drop.primary_side_effects_data
                .expect("side effects metadata")
                .is_side_effects_array_in_json
        );
    }
}
