//! Service flag parsing corresponding to pinned `pkg/cli/cli_impl.go`.
//!
//! Requests supply entries, stdin contents, working directory, node paths,
//! plugins, and write settings separately. Parsing never reads the environment,
//! consumes stdin, invokes the CLI, or infers the command from its flags.
//!
//! The native `BuildOptions.main_fields` vector cannot distinguish omitted main
//! fields from `--main-fields=`. Both parse to an empty vector, which the native
//! API currently interprets as platform defaults.

use std::collections::HashMap;

use crate::api::{
    AbsPaths, BuildEntryPoint, BuildFormat, BuildJsx, BuildLegalComments, BuildOptions,
    BuildPlatform, BuildSourceMap, BuildSourcesContent, BuildStdin, BuildTreeShaking, Engine,
    EngineName, Loader, LogLevel, Packages, Target, TransformOptions,
};
use crate::internal::cli_helpers;

#[derive(Clone, Debug)]
pub struct LogSettings {
    pub color: Option<bool>,
    pub level: LogLevel,
    pub limit: usize,
    pub overrides: HashMap<String, LogLevel>,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            color: None,
            level: LogLevel::Info,
            limit: 6,
            overrides: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParsedBuildFlags {
    pub options: BuildOptions,
    pub log_settings: LogSettings,
}

#[derive(Clone, Debug)]
pub struct ParsedTransformFlags {
    pub options: TransformOptions,
    pub log_settings: LogSettings,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Build,
    Transform,
}

/// Parse build flags with the public CLI parser's external-mode semantics.
///
/// # Errors
/// Returns the original flag error text, without CLI notes or formatting.
pub fn parse_build_flags(flags: &[String]) -> Result<ParsedBuildFlags, String> {
    let (options, _, log_settings) = parse_flags(flags, Mode::Build)?;
    Ok(ParsedBuildFlags {
        options,
        log_settings,
    })
}

/// Parse transform flags and apply the upstream API's sourcefile default.
///
/// # Errors
/// Returns the original flag error text, without CLI notes or formatting.
pub fn parse_transform_flags(flags: &[String]) -> Result<ParsedTransformFlags, String> {
    let (_, mut options, log_settings) = parse_flags(flags, Mode::Transform)?;
    if options.sourcefile.is_empty() {
        options.sourcefile = "<stdin>".into();
    }
    Ok(ParsedTransformFlags {
        options,
        log_settings,
    })
}

fn parse_flags(
    flags: &[String],
    mode: Mode,
) -> Result<(BuildOptions, TransformOptions, LogSettings), String> {
    let mut build = BuildOptions::default();
    let mut common = TransformOptions::default();
    let mut log_settings = LogSettings::default();
    let mut last_sourcemap_was_bare = false;
    for flag in flags {
        if mode == Mode::Build && parse_build_flag(&mut build, flag)? {
            continue;
        }
        if parse_common_flag(&mut common, &mut log_settings, flag, mode)? {
            if flag == "--sourcemap" {
                last_sourcemap_was_bare = true;
            } else if flag.starts_with("--sourcemap=") {
                last_sourcemap_was_bare = false;
            }
            continue;
        }
        if flag.starts_with("'--") {
            return Err(format!(
                "Unexpected single quote character before flag: {flag}"
            ));
        }
        if mode == Mode::Build && !flag.starts_with('-') {
            if let Some((output_path, input_path)) = flag.split_once('=') {
                build.entry_points_advanced.push(BuildEntryPoint {
                    output_path: output_path.into(),
                    input_path: input_path.into(),
                });
            } else {
                build.entry_points.push(flag.clone());
            }
            continue;
        }
        return Err(invalid_flag(mode, flag));
    }
    common.log_override.clone_from(&log_settings.overrides);
    if mode == Mode::Build {
        copy_common_options(common.clone(), &mut build);
        if last_sourcemap_was_bare && build.outfile.is_empty() && build.outdir.is_empty() {
            build.sourcemap = BuildSourceMap::Inline;
        }
    }
    Ok((build, common, log_settings))
}

fn copy_common_options(common: TransformOptions, build: &mut BuildOptions) {
    macro_rules! copy {
        ($($field:ident),* $(,)?) => { $(build.$field = common.$field;)* };
    }
    copy!(
        log_override,
        abs_paths,
        format,
        global_name,
        target,
        engines,
        supported,
        mangle_props,
        reserve_props,
        mangle_quoted,
        platform,
        jsx,
        jsx_factory,
        jsx_fragment,
        jsx_import_source,
        jsx_development,
        jsx_side_effects,
        define,
        pure,
        keep_names,
        line_limit,
        tree_shaking,
        minify_whitespace,
        minify_identifiers,
        minify_syntax,
        ascii_only,
        drop_console,
        drop_debugger,
        drop_labels,
        ignore_annotations,
        legal_comments,
        sourcemap,
        source_root,
        sources_content,
        tsconfig_raw,
    );
}

fn invalid_flag(mode: Mode, flag: &str) -> String {
    let command = if mode == Mode::Build {
        "build"
    } else {
        "transform"
    };
    format!("Invalid {command} flag: {}", quote_flag(flag))
}

fn invalid_value(value: &str, flag: &str) -> String {
    format!(
        "Invalid value {} in {}",
        quote_flag(value),
        quote_flag(flag)
    )
}

// Go's %q uses strconv.Quote, whose control-character escapes differ from
// Rust's Debug strings (for example, \x00 instead of \0).
fn quote_flag(value: &str) -> String {
    crate::internal::helpers::quote_go_string(value.as_bytes())
}

fn key_value<'a>(value: &'a str, flag: &str) -> Result<(&'a str, &'a str), String> {
    value
        .split_once('=')
        .ok_or_else(|| format!("Missing \"=\" in {}", quote_flag(flag)))
}

fn bool_value(flag: &str) -> Result<bool, String> {
    match flag.split_once('=') {
        None | Some((_, "true")) => Ok(true),
        Some((_, "false")) => Ok(false),
        Some((_, value)) => Err(invalid_value(value, flag)),
    }
}

fn nonnegative_integer(value: &str, flag: &str) -> Result<usize, String> {
    let integer = value
        .parse::<isize>()
        .map_err(|_| invalid_value(value, flag))?;
    usize::try_from(integer).map_err(|_| invalid_value(value, flag))
}

fn split_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(',').map(str::to_string).collect()
    }
}

fn log_level(value: &str, flag: &str) -> Result<LogLevel, String> {
    Ok(match value {
        "verbose" => LogLevel::Verbose,
        "debug" => LogLevel::Debug,
        "info" => LogLevel::Info,
        "warning" => LogLevel::Warning,
        "error" => LogLevel::Error,
        "silent" => LogLevel::Silent,
        _ => return Err(invalid_value(value, flag)),
    })
}

fn stdin_loader(value: &str, flag: &str) -> Result<Loader, String> {
    let loader = loader_value(value)?;
    if matches!(loader, Loader::File | Loader::Copy) {
        return Err(format!(
            "{} is not supported when transforming stdin",
            quote_flag(flag)
        ));
    }
    Ok(loader)
}

fn loader_value(value: &str) -> Result<Loader, String> {
    cli_helpers::parse_loader(value)
        .map_err(|_| format!("Invalid loader value: {}", quote_flag(value)))
}

fn parse_build_flag(options: &mut BuildOptions, flag: &str) -> Result<bool, String> {
    let (name, value) = flag.split_once('=').unwrap_or((flag, ""));
    match name {
        "--bundle" => options.bundle = bool_value(flag)?,
        "--preserve-symlinks" => options.preserve_symlinks = bool_value(flag)?,
        "--splitting" => options.splitting = bool_value(flag)?,
        "--allow-overwrite" => options.allow_overwrite = bool_value(flag)?,
        _ => {
            if flag == "--metafile" {
                options.metafile = true;
                return Ok(true);
            }
            if !flag.contains('=') {
                return parse_build_map_or_list(options, flag);
            }
            match name {
                "--outfile" => options.outfile = value.into(),
                "--outdir" => options.outdir = value.into(),
                "--outbase" => options.outbase = value.into(),
                "--tsconfig" => options.tsconfig = value.into(),
                "--public-path" => options.public_path = value.into(),
                "--entry-names" => options.entry_names = value.into(),
                "--chunk-names" => options.chunk_names = value.into(),
                "--asset-names" => options.asset_names = value.into(),
                "--resolve-extensions" => options.resolve_extensions = split_list(value),
                "--main-fields" => options.main_fields = split_list(value),
                "--conditions" => options.conditions = Some(split_list(value)),
                "--sourcefile" => {
                    options
                        .stdin
                        .get_or_insert_with(BuildStdin::default)
                        .sourcefile = value.into();
                }
                "--loader" => {
                    options.stdin.get_or_insert_with(BuildStdin::default).loader =
                        stdin_loader(value, flag)?;
                }
                "--packages" => {
                    options.packages = match value {
                        "bundle" => Packages::Bundle,
                        "external" => Packages::External,
                        _ => return Err(invalid_value(value, flag)),
                    }
                }
                _ => return parse_build_map_or_list(options, flag),
            }
        }
    }
    Ok(true)
}

fn parse_build_map_or_list(options: &mut BuildOptions, flag: &str) -> Result<bool, String> {
    if let Some(value) = flag.strip_prefix("--external:") {
        options.external.push(value.into());
    } else if let Some(value) = flag.strip_prefix("--inject:") {
        options.inject.push(value.into());
    } else if let Some(value) = flag.strip_prefix("--alias:") {
        let (key, value) = key_value(value, flag)?;
        options.alias.insert(key.into(), value.into());
    } else if let Some(value) = flag.strip_prefix("--loader:") {
        let (key, value) = key_value(value, flag)?;
        options.loader.insert(key.into(), loader_value(value)?);
    } else if let Some(value) = flag.strip_prefix("--out-extension:") {
        let (key, value) = key_value(value, flag)?;
        options.out_extension.insert(key.into(), value.into());
    } else if let Some(value) = flag.strip_prefix("--banner:") {
        let (key, value) = key_value(value, flag)?;
        match key {
            "js" => options.banner = value.into(),
            "css" => options.css_banner = value.into(),
            _ => return Err(invalid_flag(Mode::Build, flag)),
        }
    } else if let Some(value) = flag.strip_prefix("--footer:") {
        let (key, value) = key_value(value, flag)?;
        match key {
            "js" => options.footer = value.into(),
            "css" => options.css_footer = value.into(),
            _ => return Err(invalid_flag(Mode::Build, flag)),
        }
    } else {
        return Ok(false);
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
fn parse_common_flag(
    options: &mut TransformOptions,
    logs: &mut LogSettings,
    flag: &str,
    mode: Mode,
) -> Result<bool, String> {
    let (name, value) = flag.split_once('=').unwrap_or((flag, ""));
    match name {
        "--minify" => {
            let value = bool_value(flag)?;
            options.minify_syntax = value;
            options.minify_whitespace = value;
            options.minify_identifiers = value;
        }
        "--minify-syntax" => options.minify_syntax = bool_value(flag)?,
        "--minify-whitespace" => options.minify_whitespace = bool_value(flag)?,
        "--minify-identifiers" => options.minify_identifiers = bool_value(flag)?,
        "--mangle-quoted" => options.mangle_quoted = bool_value(flag)?,
        "--ignore-annotations" => options.ignore_annotations = bool_value(flag)?,
        "--keep-names" => options.keep_names = bool_value(flag)?,
        "--jsx-dev" => options.jsx_development = bool_value(flag)?,
        "--jsx-side-effects" => options.jsx_side_effects = bool_value(flag)?,
        "--color" => logs.color = Some(bool_value(flag)?),
        "--sources-content" => {
            options.sources_content = if bool_value(flag)? {
                BuildSourcesContent::Include
            } else {
                BuildSourcesContent::Exclude
            }
        }
        "--tree-shaking" => {
            options.tree_shaking = if bool_value(flag)? {
                BuildTreeShaking::Enabled
            } else {
                BuildTreeShaking::Disabled
            }
        }
        _ => {
            if flag == "--sourcemap" {
                options.sourcemap = if mode == Mode::Build {
                    BuildSourceMap::Linked
                } else {
                    BuildSourceMap::Inline
                };
                return Ok(true);
            }
            if let Some(value) = flag.strip_prefix("--drop:") {
                match value {
                    "console" => options.drop_console = true,
                    "debugger" => options.drop_debugger = true,
                    _ => return Err(invalid_value(value, flag)),
                }
                return Ok(true);
            }
            if let Some(value) = flag.strip_prefix("--pure:") {
                options.pure.push(value.into());
                return Ok(true);
            }
            if let Some(value) = flag.strip_prefix("--define:") {
                let (key, value) = key_value(value, flag)?;
                options.define.insert(key.into(), value.into());
                return Ok(true);
            }
            if let Some(value) = flag.strip_prefix("--log-override:") {
                let (key, value) = key_value(value, flag)?;
                logs.overrides.insert(key.into(), log_level(value, flag)?);
                return Ok(true);
            }
            if let Some(value) = flag.strip_prefix("--supported:") {
                let (key, _) = key_value(value, flag)?;
                options.supported.insert(key.into(), bool_value(flag)?);
                return Ok(true);
            }
            if !flag.contains('=') {
                return Ok(false);
            }
            match name {
                "--mangle-props" => options.mangle_props = value.into(),
                "--reserve-props" => options.reserve_props = value.into(),
                "--drop-labels" => options.drop_labels = split_list(value),
                "--source-root" => options.source_root = value.into(),
                "--sourcefile" => options.sourcefile = value.into(),
                "--loader" => options.loader = stdin_loader(value, flag)?,
                "--global-name" => options.global_name = value.into(),
                "--tsconfig-raw" => options.tsconfig_raw = value.into(),
                "--jsx-factory" => options.jsx_factory = value.into(),
                "--jsx-fragment" => options.jsx_fragment = value.into(),
                "--jsx-import-source" => options.jsx_import_source = value.into(),
                "--banner" if mode == Mode::Transform => options.banner = value.into(),
                "--footer" if mode == Mode::Transform => options.footer = value.into(),
                "--log-level" => logs.level = log_level(value, flag)?,
                "--log-limit" => logs.limit = nonnegative_integer(value, flag)?,
                "--line-limit" => options.line_limit = nonnegative_integer(value, flag)?,
                "--target" => (options.target, options.engines) = parse_targets(value, flag)?,
                "--sourcemap" => {
                    options.sourcemap = match value {
                        "linked" => BuildSourceMap::Linked,
                        "inline" => BuildSourceMap::Inline,
                        "external" => BuildSourceMap::External,
                        "both" => BuildSourceMap::InlineAndExternal,
                        _ => return Err(invalid_value(value, flag)),
                    }
                }
                "--charset" => {
                    options.ascii_only = match value {
                        "ascii" => true,
                        "utf8" => false,
                        _ => return Err(invalid_value(value, flag)),
                    }
                }
                "--legal-comments" => {
                    options.legal_comments = match value {
                        "none" => BuildLegalComments::None,
                        "inline" => BuildLegalComments::Inline,
                        "eof" => BuildLegalComments::EndOfFile,
                        "linked" => BuildLegalComments::Linked,
                        "external" => BuildLegalComments::External,
                        _ => return Err(invalid_value(value, flag)),
                    }
                }
                "--format" => {
                    options.format = match value {
                        "iife" => BuildFormat::Iife,
                        "cjs" => BuildFormat::CommonJs,
                        "esm" => BuildFormat::EsModule,
                        _ => return Err(invalid_value(value, flag)),
                    }
                }
                "--platform" => {
                    options.platform = match value {
                        "browser" => BuildPlatform::Browser,
                        "node" => BuildPlatform::Node,
                        "neutral" => BuildPlatform::Neutral,
                        _ => return Err(invalid_value(value, flag)),
                    }
                }
                "--jsx" => {
                    options.jsx = match value {
                        "transform" => BuildJsx::Transform,
                        "preserve" => BuildJsx::Preserve,
                        "automatic" => BuildJsx::Automatic,
                        _ => return Err(invalid_value(value, flag)),
                    }
                }
                "--abs-paths" => {
                    let mut paths = AbsPaths::default();
                    for value in split_list(value) {
                        paths |= match value.as_str() {
                            "code" => AbsPaths::CODE,
                            "log" => AbsPaths::LOG,
                            "metafile" => AbsPaths::METAFILE,
                            _ => return Err(invalid_value(&value, flag)),
                        };
                    }
                    options.abs_paths = paths;
                }
                _ => return Ok(false),
            }
        }
    }
    Ok(true)
}

fn parse_targets(value: &str, flag: &str) -> Result<(Target, Vec<Engine>), String> {
    const ENGINES: &[(&str, EngineName)] = &[
        ("chrome", EngineName::Chrome),
        ("deno", EngineName::Deno),
        ("edge", EngineName::Edge),
        ("firefox", EngineName::Firefox),
        ("hermes", EngineName::Hermes),
        ("ie", EngineName::Ie),
        ("ios", EngineName::Ios),
        ("node", EngineName::Node),
        ("opera", EngineName::Opera),
        ("rhino", EngineName::Rhino),
        ("safari", EngineName::Safari),
    ];
    let mut target = Target::Default;
    let mut engines = Vec::new();
    for value in split_list(value) {
        let es_target = match value.to_ascii_lowercase().as_str() {
            "esnext" => Some(Target::EsNext),
            "es5" => Some(Target::Es5),
            "es6" | "es2015" => Some(Target::Es2015),
            "es2016" => Some(Target::Es2016),
            "es2017" => Some(Target::Es2017),
            "es2018" => Some(Target::Es2018),
            "es2019" => Some(Target::Es2019),
            "es2020" => Some(Target::Es2020),
            "es2021" => Some(Target::Es2021),
            "es2022" => Some(Target::Es2022),
            "es2023" => Some(Target::Es2023),
            "es2024" => Some(Target::Es2024),
            "es2025" => Some(Target::Es2025),
            _ => None,
        };
        if let Some(value) = es_target {
            target = value;
        } else if let Some((name, engine)) =
            ENGINES.iter().find(|(name, _)| value.starts_with(name))
        {
            let version = &value[name.len()..];
            if version.is_empty() {
                return Err(format!(
                    "Target {} is missing a version number in {}",
                    quote_flag(&value),
                    quote_flag(flag)
                ));
            }
            engines.push(Engine {
                name: *engine,
                version: version.into(),
            });
        } else {
            return Err(format!(
                "Invalid target {} in {}",
                quote_flag(&value),
                quote_flag(flag)
            ));
        }
    }
    Ok((target, engines))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn flag_errors_use_go_control_character_and_unicode_quoting() {
        for (value, quoted) in [
            ("\0", "\"\\x00\""),
            ("\u{7f}", "\"\\x7f\""),
            ("\u{7}\u{b}\u{c}", "\"\\a\\v\\f\""),
            ("\u{a0}\u{2028}", "\"\\u00a0\\u2028\""),
            ("é🙂\u{301}", "\"é🙂\u{301}\""),
            ("\u{1c89}", "\"\\u1c89\""),
        ] {
            assert_eq!(quote_flag(value), quoted);
            let flag = format!("--format={value}");
            assert_eq!(
                parse_transform_flags(&flags(&[&flag])).unwrap_err(),
                format!("Invalid value {quoted} in {}", quote_flag(&flag))
            );
            assert_eq!(
                parse_build_flags(&flags(&[&format!("--loader={value}")])).unwrap_err(),
                format!("Invalid loader value: {quoted}")
            );
        }
    }

    #[test]
    fn wrapper_default_flags_parse_with_explicit_modes() {
        let build = parse_build_flags(&flags(&["--log-level=warning", "--log-limit=0"])).unwrap();
        assert!(!build.options.bundle);
        assert!(build.options.entry_points.is_empty());
        assert!(build.options.node_paths.is_empty());
        assert!(build.options.abs_working_dir.is_empty());
        assert!(build.options.stdin.is_none());
        assert!(!build.options.write);
        assert_eq!(build.log_settings.level, LogLevel::Warning);
        assert_eq!(build.log_settings.limit, 0);
        let transform =
            parse_transform_flags(&flags(&["--log-level=silent", "--log-limit=0"])).unwrap();
        assert_eq!(transform.options.sourcefile, "<stdin>");
        assert_eq!(transform.log_settings.level, LogLevel::Silent);
        assert_eq!(transform.log_settings.limit, 0);
        let defaults = LogSettings::default();
        assert_eq!(defaults.color, None);
        assert_eq!(defaults.level, LogLevel::Info);
        assert_eq!(defaults.limit, 6);
    }

    #[test]
    fn build_sourcemap_tracks_the_final_flag_and_output_path() {
        for (values, expected) in [
            (vec!["--sourcemap"], BuildSourceMap::Inline),
            (
                vec!["--sourcemap", "--outfile=out.js"],
                BuildSourceMap::Linked,
            ),
            (vec!["--outdir=out", "--sourcemap"], BuildSourceMap::Linked),
            (
                vec!["--sourcemap=external", "--sourcemap"],
                BuildSourceMap::Inline,
            ),
            (
                vec!["--sourcemap", "--sourcemap=linked"],
                BuildSourceMap::Linked,
            ),
            (
                vec!["--outfile=out.js", "--sourcemap", "--outfile="],
                BuildSourceMap::Inline,
            ),
            (vec!["--sourcemap=both"], BuildSourceMap::InlineAndExternal),
        ] {
            assert_eq!(
                parse_build_flags(&flags(&values))
                    .unwrap()
                    .options
                    .sourcemap,
                expected
            );
        }
        let parsed = parse_build_flags(&flags(&["--metafile"])).unwrap();
        assert!(parsed.options.metafile);
        assert!(parse_build_flags(&flags(&["--metafile=meta.json"])).is_err());
    }

    #[test]
    fn transform_sourcemaps_receive_the_api_sourcefile_default() {
        for flag in [
            "--sourcemap",
            "--sourcemap=external",
            "--sourcemap=both",
            "--sourcemap=linked",
        ] {
            for sourcefile in [None, Some("--sourcefile="), Some("--sourcefile=virtual.ts")] {
                let mut values = vec![flag];
                if let Some(sourcefile) = sourcefile {
                    values.push(sourcefile);
                }
                let parsed = parse_transform_flags(&flags(&values)).unwrap();
                assert_eq!(
                    parsed.options.sourcefile,
                    if sourcefile == Some("--sourcefile=virtual.ts") {
                        "virtual.ts"
                    } else {
                        "<stdin>"
                    }
                );
            }
        }
        let parsed = parse_transform_flags(&flags(&["--sourcemap=external"])).unwrap();
        let result = crate::api::transform("let x = 1", parsed.options);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(!result.map.is_empty());
    }

    #[test]
    fn conditions_keep_absent_empty_and_empty_list_members_distinct() {
        assert!(parse_build_flags(&[]).unwrap().options.conditions.is_none());
        assert_eq!(
            parse_build_flags(&flags(&["--conditions="]))
                .unwrap()
                .options
                .conditions,
            Some(vec![])
        );
        assert_eq!(
            parse_build_flags(&flags(&["--conditions=first", "--conditions=,custom,"]))
                .unwrap()
                .options
                .conditions,
            Some(flags(&["", "custom", ""]))
        );
        assert!(parse_transform_flags(&flags(&["--conditions="])).is_err());
    }

    #[test]
    fn maps_accumulate_lists_append_and_scalar_lists_replace() {
        let values = flags(&[
            "--define:foo=1",
            "--define:bar=2",
            "--define:foo=3=4",
            "--pure:one",
            "--pure:",
            "--pure:one",
            "--external:pkg",
            "--external:pkg",
            "--inject:one.js",
            "--inject:two.js",
            "--alias:pkg=one",
            "--alias:pkg=two",
            "--loader:.custom=text",
            "--loader:.custom=json",
            "--loader:.css=local-css",
            "--out-extension:.js=.mjs",
            "--out-extension:.css=.style.css",
            "--main-fields=main,module",
            "--main-fields=",
            "--resolve-extensions=.jsx,,.js",
            "--drop-labels=one,two",
            "--drop-labels=last",
            "--supported:arrow=true",
            "--supported:arrow=false",
            "--banner:js=before",
            "--banner:css=styles",
            "--banner:js=last",
            "--footer:css=end",
        ]);
        let parsed = parse_build_flags(&values).unwrap();
        let options = parsed.options;
        assert_eq!(
            options.define,
            HashMap::from([("foo".into(), "3=4".into()), ("bar".into(), "2".into())])
        );
        assert_eq!(options.pure, flags(&["one", "", "one"]));
        assert_eq!(options.external, flags(&["pkg", "pkg"]));
        assert_eq!(options.inject, flags(&["one.js", "two.js"]));
        assert_eq!(options.alias["pkg"], "two");
        assert_eq!(options.loader[".custom"], Loader::Json);
        assert_eq!(options.loader[".css"], Loader::LocalCss);
        assert!(options.main_fields.is_empty());
        assert_eq!(options.resolve_extensions, flags(&[".jsx", "", ".js"]));
        assert_eq!(options.drop_labels, flags(&["last"]));
        assert!(!options.supported["arrow"]);
        assert_eq!(options.banner, "last");
        assert_eq!(options.css_banner, "styles");
        assert_eq!(options.css_footer, "end");
    }

    #[test]
    fn sourcefile_and_loader_create_stdin_without_injecting_contents_or_cwd() {
        for values in [
            vec!["--sourcefile="],
            vec!["--loader=tsx"],
            vec!["--sourcefile=virtual.tsx", "--loader=tsx"],
        ] {
            let stdin = parse_build_flags(&flags(&values))
                .unwrap()
                .options
                .stdin
                .unwrap();
            assert!(stdin.contents.is_empty());
            assert!(stdin.resolve_dir.is_empty());
        }
        for mode in [Mode::Build, Mode::Transform] {
            for value in ["--loader=file", "--loader=copy"] {
                assert_eq!(
                    parse_flags(&flags(&[value]), mode).unwrap_err(),
                    format!("{value:?} is not supported when transforming stdin")
                );
            }
        }
    }

    #[test]
    fn log_flags_validate_and_repeated_values_replace() {
        let values = flags(&[
            "--color",
            "--color=false",
            "--color=true",
            "--log-level=debug",
            "--log-level=warning",
            "--log-limit=+0",
            "--log-override:custom=error",
            "--log-override:custom=silent",
            "--log-override:other=debug",
        ]);
        let parsed = parse_build_flags(&values).unwrap();
        assert_eq!(parsed.log_settings.color, Some(true));
        assert_eq!(parsed.log_settings.level, LogLevel::Warning);
        assert_eq!(parsed.log_settings.limit, 0);
        assert_eq!(parsed.log_settings.overrides["custom"], LogLevel::Silent);
        assert_eq!(parsed.log_settings.overrides, parsed.options.log_override);
        for (flag, expected) in [
            ("--color=", "Invalid value \"\" in \"--color=\""),
            (
                "--log-level=warnings",
                "Invalid value \"warnings\" in \"--log-level=warnings\"",
            ),
            (
                "--log-limit=-1",
                "Invalid value \"-1\" in \"--log-limit=-1\"",
            ),
            (
                "--log-limit=1.5",
                "Invalid value \"1.5\" in \"--log-limit=1.5\"",
            ),
            (
                "--log-override:custom",
                "Missing \"=\" in \"--log-override:custom\"",
            ),
            (
                "--log-override:custom=warn",
                "Invalid value \"warn\" in \"--log-override:custom=warn\"",
            ),
        ] {
            assert_eq!(
                parse_transform_flags(&flags(&[flag])).unwrap_err(),
                expected
            );
        }
        if usize::BITS == 64 {
            assert_eq!(
                parse_transform_flags(&flags(&["--log-limit=2147483648"]))
                    .unwrap()
                    .log_settings
                    .limit,
                2_147_483_648
            );
        }
    }

    #[test]
    fn target_flags_preserve_all_engines_and_replace_previous_targets() {
        let parsed = parse_transform_flags(&flags(&[
            "--target=es5,chrome80",
            "--target=ES6,node12,node14,ios13",
        ]))
        .unwrap();
        assert_eq!(parsed.options.target, Target::Es2015);
        assert_eq!(
            parsed.options.engines,
            vec![
                Engine {
                    name: EngineName::Node,
                    version: "12".into()
                },
                Engine {
                    name: EngineName::Node,
                    version: "14".into()
                },
                Engine {
                    name: EngineName::Ios,
                    version: "13".into()
                }
            ]
        );
        assert_eq!(
            parse_transform_flags(&flags(&["--target="]))
                .unwrap()
                .options
                .target,
            Target::Default
        );
        assert_eq!(
            parse_transform_flags(&flags(&["--target=node"])).unwrap_err(),
            "Target \"node\" is missing a version number in \"--target=node\""
        );
        assert_eq!(
            parse_transform_flags(&flags(&["--target=es2022,"])).unwrap_err(),
            "Invalid target \"\" in \"--target=es2022,\""
        );
    }

    #[test]
    fn modes_reject_unmapped_flags_and_keep_external_error_text() {
        for flag in [
            "--bundle",
            "--outdir=out",
            "--metafile",
            "--loader:.css=css",
            "--external:pkg",
            "entry.js",
        ] {
            assert_eq!(
                parse_transform_flags(&flags(&[flag])).unwrap_err(),
                format!("Invalid transform flag: {flag:?}")
            );
        }
        for flag in [
            "--watch",
            "--watch-delay=10",
            "--analyze",
            "--mangle-cache=cache.json",
            "--banner=wrong-mode",
            "--banner:html=unmapped",
        ] {
            assert_eq!(
                parse_build_flags(&flags(&[flag])).unwrap_err(),
                format!("Invalid build flag: {flag:?}")
            );
        }
        assert_eq!(
            parse_transform_flags(&flags(&["'--minify'"])).unwrap_err(),
            "Unexpected single quote character before flag: '--minify'"
        );
        assert_eq!(
            parse_build_flags(&flags(&["--loader:.js=wat"])).unwrap_err(),
            "Invalid loader value: \"wat\""
        );
        assert_eq!(
            parse_build_flags(&flags(&["--alias:missing"])).unwrap_err(),
            "Missing \"=\" in \"--alias:missing\""
        );
    }
}
