use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
};

use esbuild_rs::{
    api::{
        AnalyzeMetafileOptions, BuildEntryPoint, BuildFormat, BuildJsx, BuildLegalComments,
        BuildOptions, BuildPlatform, BuildSourceMap, BuildSourcesContent, BuildStdin,
        BuildTreeShaking, Loader, Packages, TransformOptions, analyze_metafile, build, transform,
    },
    internal::cli_helpers,
};

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match run(&arguments) {
        Ok(output) => {
            if let Err(error) = write_output(output) {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

enum Output {
    Text(String),
    Code(Vec<u8>),
    WithStderr { output: Box<Output>, stderr: String },
}

fn write_output(output: Output) -> io::Result<()> {
    match output {
        Output::Text(text) => io::stdout().write_all(text.as_bytes()),
        Output::Code(code) => io::stdout().write_all(&code),
        Output::WithStderr { output, stderr } => {
            io::stderr().write_all(stderr.as_bytes())?;
            write_output(*output)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AnalyzeMode {
    #[default]
    Disabled,
    Enabled,
    Verbose,
}

#[allow(clippy::too_many_lines)]
fn run(arguments: &[String]) -> Result<Output, String> {
    run_with_stdin(arguments, None)
}

fn node_paths_from_environment() -> Vec<String> {
    env::var_os("NODE_PATH").map_or_else(Vec::new, |paths| {
        env::split_paths(&paths)
            .filter_map(|path| path.to_str().map(str::to_string))
            .collect()
    })
}

fn run_with_stdin(arguments: &[String], stdin_override: Option<&[u8]>) -> Result<Output, String> {
    run_with_stdin_and_node_paths(arguments, stdin_override, node_paths_from_environment())
}

#[allow(clippy::too_many_lines)]
fn run_with_stdin_and_node_paths(
    arguments: &[String],
    stdin_override: Option<&[u8]>,
    node_paths: Vec<String>,
) -> Result<Output, String> {
    let mut options = TransformOptions::default();
    let mut input_paths = Vec::new();
    let mut bundle = false;
    let mut outdir = String::new();
    let mut outfile = String::new();
    let mut outbase = String::new();
    let mut tsconfig = String::new();
    let mut tsconfig_raw = String::new();
    let mut metafile_path = String::new();
    let mut analyze = AnalyzeMode::Disabled;
    let mut format = BuildFormat::Default;
    let mut platform = BuildPlatform::Browser;
    let mut global_name = String::new();
    let mut public_path = String::new();
    let mut entry_names = String::new();
    let mut chunk_names = String::new();
    let mut asset_names = String::new();
    let mut splitting = false;
    let mut preserve_symlinks = false;
    let mut allow_overwrite = false;
    let mut sourcemap = BuildSourceMap::None;
    let mut bare_sourcemap = false;
    let mut source_root = String::new();
    let mut sources_content = BuildSourcesContent::Include;
    let mut legal_comments = BuildLegalComments::Default;
    let mut tree_shaking = BuildTreeShaking::Default;
    let mut jsx = BuildJsx::Transform;
    let mut jsx_factory = String::new();
    let mut jsx_fragment = String::new();
    let mut jsx_import_source = String::new();
    let mut jsx_development = false;
    let mut jsx_side_effects = false;
    let mut external = Vec::new();
    let mut aliases = HashMap::new();
    let mut packages = Packages::Bundle;
    let mut build_loaders = HashMap::new();
    let mut out_extensions = HashMap::new();
    let mut defines = HashMap::new();
    let mut pure = Vec::new();
    let mut keep_names = false;
    let mut css_banner = String::new();
    let mut css_footer = String::new();
    let mut main_fields = Vec::new();
    let mut resolve_extensions = Vec::new();
    let mut conditions = Vec::new();
    for argument in arguments {
        if argument == "--help" || argument == "-h" {
            return Ok(Output::Text(help_text()));
        }
        if argument == "--version" {
            return Ok(Output::Text(format!("{}\n", env!("CARGO_PKG_VERSION"))));
        }
        if argument == "--minify" {
            options.minify_whitespace = true;
            options.minify_identifiers = true;
            options.minify_syntax = true;
            continue;
        }
        if argument == "--bundle" {
            bundle = true;
            continue;
        }
        if argument == "--analyze" {
            analyze = AnalyzeMode::Enabled;
            continue;
        }
        if argument == "--analyze=verbose" {
            analyze = AnalyzeMode::Verbose;
            continue;
        }
        if argument == "--splitting" {
            splitting = true;
            continue;
        }
        if argument == "--preserve-symlinks" {
            preserve_symlinks = true;
            continue;
        }
        if argument == "--allow-overwrite" {
            allow_overwrite = true;
            continue;
        }
        if argument == "--drop:debugger" {
            options.drop_debugger = true;
            continue;
        }
        if argument == "--drop:console" {
            options.drop_console = true;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--drop-labels=") {
            if value.is_empty() || value.split(',').any(str::is_empty) {
                return Err("Invalid empty label in \"--drop-labels\"".into());
            }
            options.drop_labels = value.split(',').map(str::to_string).collect();
            continue;
        }
        if argument == "--ignore-annotations" {
            options.ignore_annotations = true;
            continue;
        }
        if argument == "--sourcemap" {
            sourcemap = BuildSourceMap::Linked;
            bare_sourcemap = true;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--sourcemap=") {
            bare_sourcemap = false;
            sourcemap = match value {
                "linked" => BuildSourceMap::Linked,
                "external" => BuildSourceMap::External,
                "inline" => BuildSourceMap::Inline,
                "both" => BuildSourceMap::InlineAndExternal,
                _ => return Err(format!("Invalid source map setting {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--source-root=") {
            source_root = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--sources-content=") {
            sources_content = match value {
                "true" => BuildSourcesContent::Include,
                "false" => BuildSourcesContent::Exclude,
                _ => return Err(format!("Invalid sources content setting {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--legal-comments=") {
            legal_comments = match value {
                "none" => BuildLegalComments::None,
                "inline" => BuildLegalComments::Inline,
                "eof" => BuildLegalComments::EndOfFile,
                "linked" => BuildLegalComments::Linked,
                "external" => BuildLegalComments::External,
                _ => return Err(format!("Invalid legal comments setting {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--tree-shaking=") {
            tree_shaking = match value {
                "true" => BuildTreeShaking::Enabled,
                "false" => BuildTreeShaking::Disabled,
                _ => return Err(format!("Invalid tree shaking setting {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--jsx=") {
            jsx = match value {
                "transform" => BuildJsx::Transform,
                "preserve" => BuildJsx::Preserve,
                "automatic" => BuildJsx::Automatic,
                _ => return Err(format!("Invalid JSX setting {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--jsx-factory=") {
            jsx_factory = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--jsx-fragment=") {
            jsx_fragment = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--jsx-import-source=") {
            jsx_import_source = value.into();
            continue;
        }
        if argument == "--jsx-dev" {
            jsx_development = true;
            continue;
        }
        if argument == "--jsx-side-effects" {
            jsx_side_effects = true;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--jsx-side-effects=") {
            jsx_side_effects = match value {
                "true" => true,
                "false" => false,
                _ => return Err(format!("Invalid JSX side effects setting {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--outdir=") {
            outdir = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--outfile=") {
            outfile = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--outbase=") {
            outbase = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--tsconfig=") {
            tsconfig = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--tsconfig-raw=") {
            tsconfig_raw = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--metafile=") {
            if value.is_empty() {
                return Err("Invalid empty metafile path".into());
            }
            metafile_path = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--format=") {
            format = match value {
                "iife" => BuildFormat::Iife,
                "cjs" => BuildFormat::CommonJs,
                "esm" => BuildFormat::EsModule,
                _ => return Err(format!("Invalid format {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--platform=") {
            platform = match value {
                "browser" => BuildPlatform::Browser,
                "node" => BuildPlatform::Node,
                "neutral" => BuildPlatform::Neutral,
                _ => return Err(format!("Invalid platform {value:?}")),
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--global-name=") {
            global_name = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--public-path=") {
            public_path = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--entry-names=") {
            entry_names = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--chunk-names=") {
            chunk_names = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--asset-names=") {
            asset_names = value.into();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--main-fields=") {
            main_fields = if value.is_empty() {
                Vec::new()
            } else {
                value.split(',').map(str::to_string).collect()
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--resolve-extensions=") {
            resolve_extensions = if value.is_empty() {
                Vec::new()
            } else {
                value.split(',').map(str::to_string).collect()
            };
            continue;
        }
        if let Some(value) = argument.strip_prefix("--conditions=") {
            conditions = value
                .split(',')
                .filter(|condition| !condition.is_empty())
                .map(str::to_string)
                .collect();
            continue;
        }
        if let Some(value) = argument.strip_prefix("--external:") {
            external.push(value.into());
            continue;
        }
        if let Some(value) = argument.strip_prefix("--alias:") {
            let Some((old, new)) = value.split_once('=') else {
                return Err(format!("Missing \"=\" in {argument:?}"));
            };
            aliases.insert(old.into(), new.into());
            continue;
        }
        if let Some(value) = argument.strip_prefix("--packages=") {
            packages = match value {
                "bundle" => Packages::Bundle,
                "external" => Packages::External,
                _ => return Err(format!("Invalid packages setting {value:?}")),
            };
            continue;
        }
        if argument == "--minify-whitespace" {
            options.minify_whitespace = true;
            continue;
        }
        if argument == "--minify-identifiers" {
            options.minify_identifiers = true;
            continue;
        }
        if argument == "--minify-syntax" {
            options.minify_syntax = true;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--loader:") {
            let Some((extension, loader)) = value.split_once('=') else {
                return Err(format!(
                    "Missing \"=\" in {argument:?}\n\n\
                     You need to specify the file extension that the loader applies to. \
                     For example, \"--loader:.js=jsx\" applies the \"jsx\" loader to files \
                     with the \".js\" extension."
                ));
            };
            let loader = cli_helpers::parse_loader(loader)
                .map_err(|error| format!("{}\n\n{}", error.text, error.note))?;
            build_loaders.insert(extension.into(), loader);
            continue;
        }
        if let Some(value) = argument.strip_prefix("--out-extension:") {
            let Some((kind, extension)) = value.split_once('=') else {
                return Err(format!("Missing \"=\" in {argument:?}"));
            };
            out_extensions.insert(kind.into(), extension.into());
            continue;
        }
        if let Some(value) = argument.strip_prefix("--define:") {
            let Some((key, value)) = value.split_once('=') else {
                return Err(format!("Missing \"=\" in {argument:?}"));
            };
            defines.insert(key.into(), value.into());
            continue;
        }
        if let Some(value) = argument.strip_prefix("--pure:") {
            if value.is_empty() {
                return Err("Invalid empty pure call target".into());
            }
            pure.push(value.into());
            continue;
        }
        if argument == "--keep-names" {
            keep_names = true;
            continue;
        }
        if let Some(loader) = argument.strip_prefix("--loader=") {
            options.loader = parse_loader(loader)?;
            continue;
        }
        if let Some(charset) = argument.strip_prefix("--charset=") {
            options.ascii_only = match charset {
                "ascii" => true,
                "utf8" => false,
                _ => return Err(format!("Invalid charset {charset:?}")),
            };
            continue;
        }
        if let Some(limit) = argument.strip_prefix("--line-limit=") {
            options.line_limit = limit
                .parse()
                .map_err(|_| format!("Invalid line limit {limit:?}"))?;
            continue;
        }
        if let Some(sourcefile) = argument.strip_prefix("--sourcefile=") {
            options.sourcefile = sourcefile.into();
            continue;
        }
        if let Some(banner) = argument.strip_prefix("--banner:js=") {
            options.banner = banner.into();
            continue;
        }
        if let Some(banner) = argument.strip_prefix("--banner:css=") {
            css_banner = banner.into();
            continue;
        }
        if let Some(banner) = argument.strip_prefix("--banner=") {
            options.banner = banner.into();
            continue;
        }
        if let Some(footer) = argument.strip_prefix("--footer:js=") {
            options.footer = footer.into();
            continue;
        }
        if let Some(footer) = argument.strip_prefix("--footer:css=") {
            css_footer = footer.into();
            continue;
        }
        if let Some(footer) = argument.strip_prefix("--footer=") {
            options.footer = footer.into();
            continue;
        }
        if argument.starts_with('-') {
            return Err(format!("Invalid option {argument:?}"));
        }
        input_paths.push(argument.clone());
    }

    let use_build_api = bundle
        || !outdir.is_empty()
        || !outfile.is_empty()
        || !metafile_path.is_empty()
        || analyze != AnalyzeMode::Disabled
        || input_paths.len() > 1
        || input_paths.iter().any(|path| path.contains('='));
    if use_build_api {
        if !outdir.is_empty() && !outfile.is_empty() {
            return Err("Cannot use both \"--outfile\" and \"--outdir\"".into());
        }
        if !input_paths.is_empty() && options.loader != Loader::None {
            return Err("\"loader\" without extension only applies when reading from stdin".into());
        }
        if !metafile_path.is_empty() && outdir.is_empty() && outfile.is_empty() {
            return Err("Cannot use \"--metafile\" without an output path".into());
        }
        let mut entry_points = Vec::new();
        let mut entry_points_advanced = Vec::new();
        for input_path in input_paths {
            if let Some((output_path, input_path)) = input_path.split_once('=') {
                if output_path.is_empty() || input_path.is_empty() {
                    return Err("Invalid advanced entry point".into());
                }
                entry_points_advanced.push(BuildEntryPoint {
                    input_path: input_path.into(),
                    output_path: output_path.into(),
                });
            } else {
                entry_points.push(input_path);
            }
        }
        let canonical_input_paths = entry_points
            .iter()
            .chain(entry_points_advanced.iter().map(|entry| &entry.input_path))
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect::<std::collections::HashSet<_>>();
        let stdin = if entry_points.is_empty() && entry_points_advanced.is_empty() {
            let mut contents = Vec::new();
            if let Some(stdin) = stdin_override {
                contents.extend_from_slice(stdin);
            } else {
                io::stdin()
                    .read_to_end(&mut contents)
                    .map_err(|error| format!("Could not read stdin: {error}"))?;
            }
            Some(BuildStdin {
                contents: String::from_utf8(contents)
                    .map_err(|_| "Stdin must be valid UTF-8".to_string())?,
                resolve_dir: env::current_dir()
                    .ok()
                    .and_then(|path| path.to_str().map(str::to_string))
                    .unwrap_or_default(),
                sourcefile: options.sourcefile.clone(),
                loader: options.loader,
            })
        } else {
            None
        };
        let result = build(BuildOptions {
            bundle,
            entry_points,
            entry_points_advanced,
            stdin,
            outdir: outdir.clone(),
            outfile: outfile.clone(),
            outbase,
            tsconfig,
            tsconfig_raw,
            metafile: !metafile_path.is_empty() || analyze != AnalyzeMode::Disabled,
            format,
            platform,
            global_name,
            public_path,
            entry_names,
            chunk_names,
            asset_names,
            sourcemap,
            source_root,
            sources_content,
            legal_comments,
            line_limit: options.line_limit,
            tree_shaking,
            jsx,
            jsx_factory,
            jsx_fragment,
            jsx_import_source,
            jsx_development,
            jsx_side_effects,
            splitting,
            preserve_symlinks,
            allow_overwrite,
            minify_whitespace: options.minify_whitespace,
            minify_identifiers: options.minify_identifiers,
            minify_syntax: options.minify_syntax,
            ascii_only: options.ascii_only,
            drop_console: options.drop_console,
            drop_debugger: options.drop_debugger,
            drop_labels: options.drop_labels,
            ignore_annotations: options.ignore_annotations,
            banner: options.banner,
            footer: options.footer,
            css_banner,
            css_footer,
            external,
            alias: aliases,
            packages,
            loader: build_loaders,
            out_extension: out_extensions,
            define: defines,
            pure,
            keep_names,
            main_fields,
            resolve_extensions,
            conditions,
            node_paths,
            ..BuildOptions::default()
        });
        if !result.errors.is_empty() {
            return Err(result
                .errors
                .iter()
                .map(|message| format!("error: {}", message.text))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        for warning in result.warnings {
            eprintln!("warning: {}", warning.text);
        }
        if !allow_overwrite && (!outdir.is_empty() || !outfile.is_empty()) {
            for output in &result.output_files {
                if fs::canonicalize(&output.path)
                    .ok()
                    .is_some_and(|path| canonical_input_paths.contains(&path))
                {
                    return Err(format!(
                        "Refusing to overwrite input file {:?} (use \"--allow-overwrite\" to allow this)",
                        output.path
                    ));
                }
            }
        }
        let analysis = if analyze == AnalyzeMode::Disabled {
            String::new()
        } else {
            format!(
                "{}\n",
                analyze_metafile(
                    &result.metafile,
                    AnalyzeMetafileOptions {
                        verbose: analyze == AnalyzeMode::Verbose,
                        ..AnalyzeMetafileOptions::default()
                    }
                )
            )
        };
        if outdir.is_empty() && outfile.is_empty() {
            let [output] = result.output_files.as_slice() else {
                return Err("Must use \"--outdir\" when there are multiple output files".into());
            };
            let output = Output::Code(output.contents.clone());
            return Ok(if analysis.is_empty() {
                output
            } else {
                Output::WithStderr {
                    output: Box::new(output),
                    stderr: analysis,
                }
            });
        }
        for output in result.output_files {
            let path = std::path::Path::new(&output.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
            }
            fs::write(path, output.contents)
                .map_err(|error| format!("Could not write {:?}: {error}", output.path))?;
        }
        if !metafile_path.is_empty() {
            let path = std::path::Path::new(&metafile_path);
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
            }
            fs::write(path, result.metafile)
                .map_err(|error| format!("Could not write {metafile_path:?}: {error}"))?;
        }
        let output = Output::Text(String::new());
        return Ok(if analysis.is_empty() {
            output
        } else {
            Output::WithStderr {
                output: Box::new(output),
                stderr: analysis,
            }
        });
    }

    if bare_sourcemap {
        sourcemap = BuildSourceMap::Inline;
    }
    if matches!(
        sourcemap,
        BuildSourceMap::Linked | BuildSourceMap::External | BuildSourceMap::InlineAndExternal
    ) {
        let mode = match sourcemap {
            BuildSourceMap::Linked => "linked",
            BuildSourceMap::External => "external",
            BuildSourceMap::InlineAndExternal => "both",
            _ => unreachable!(),
        };
        return Err(format!(
            "Use \"--sourcemap\" instead of \"--sourcemap={mode}\" when transforming stdin"
        ));
    }
    if input_paths.len() > 1 {
        return Err("Only one input file can be transformed at a time".into());
    }
    options.jsx = jsx;
    options.jsx_factory = jsx_factory;
    options.jsx_fragment = jsx_fragment;
    options.jsx_import_source = jsx_import_source;
    options.jsx_development = jsx_development;
    options.jsx_side_effects = jsx_side_effects;
    options.define = defines;
    options.pure = pure;
    options.keep_names = keep_names;
    options.platform = platform;
    options.legal_comments = legal_comments;
    options.sourcemap = sourcemap;
    options.source_root = source_root;
    options.sources_content = sources_content;
    options.tsconfig_raw = tsconfig_raw;
    if matches!(
        options.legal_comments,
        BuildLegalComments::Linked | BuildLegalComments::External
    ) {
        return Err("Cannot transform with linked or external legal comments".into());
    }
    let input = if let Some(path) = input_paths.pop() {
        if options.sourcefile.is_empty() {
            options.sourcefile.clone_from(&path);
        }
        if options.loader == Loader::None {
            options.loader = Loader::Default;
        }
        fs::read(&path).map_err(|error| format!("Could not read {path:?}: {error}"))?
    } else if let Some(stdin) = stdin_override {
        stdin.to_vec()
    } else {
        let mut input = Vec::new();
        io::stdin()
            .read_to_end(&mut input)
            .map_err(|error| format!("Could not read stdin: {error}"))?;
        input
    };

    let result = transform(input, options);
    if !result.errors.is_empty() {
        return Err(result
            .errors
            .iter()
            .map(|message| format!("error: {}", message.text))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    for warning in result.warnings {
        eprintln!("warning: {}", warning.text);
    }
    Ok(Output::Code(result.code))
}

fn parse_loader(loader: &str) -> Result<Loader, String> {
    let loader = cli_helpers::parse_loader(loader)
        .map_err(|error| format!("{}\n\n{}", error.text, error.note))?;
    if matches!(loader, Loader::File | Loader::Copy) {
        let name = loader_name(loader);
        return Err(format!(
            "\"--loader={name}\" is not supported when transforming stdin\n\n\
             Using esbuild to transform stdin only generates one output file, so you cannot use \
             the \"{name}\" loader since that needs to generate two output files."
        ));
    }
    Ok(loader)
}

const fn loader_name(loader: Loader) -> &'static str {
    match loader {
        Loader::Copy => "copy",
        Loader::File => "file",
        _ => "",
    }
}

fn help_text() -> String {
    format!(
        "esbuild-rs {}\n\
         Usage: esbuild [options] [input-file]\n\n\
         Options:\n\
         \x20\x20--bundle\n\
         \x20\x20--analyze[=verbose]\n\
         \x20\x20--outdir=DIR\n\
         \x20\x20--outfile=FILE\n\
         \x20\x20--outbase=DIR\n\
         \x20\x20--tsconfig=FILE\n\
         \x20\x20--tsconfig-raw=JSON\n\
         \x20\x20--metafile=FILE\n\
         \x20\x20--format=iife|cjs|esm\n\
         \x20\x20--platform=browser|node|neutral\n\
         \x20\x20--global-name=NAME\n\
         \x20\x20--public-path=PATH\n\
         \x20\x20--entry-names=TEMPLATE\n\
         \x20\x20--chunk-names=TEMPLATE\n\
         \x20\x20--asset-names=TEMPLATE\n\
         \x20\x20--splitting\n\
         \x20\x20--preserve-symlinks\n\
         \x20\x20--allow-overwrite\n\
         \x20\x20--sourcemap[=linked|external|inline|both]\n\
         \x20\x20--source-root=PATH\n\
         \x20\x20--sources-content=true|false\n\
         \x20\x20--legal-comments=none|inline|eof|linked|external\n\
         \x20\x20--tree-shaking=true|false\n\
         \x20\x20--jsx=transform|preserve|automatic\n\
         \x20\x20--jsx-factory=EXPRESSION\n\
         \x20\x20--jsx-fragment=EXPRESSION\n\
         \x20\x20--jsx-import-source=PATH\n\
         \x20\x20--jsx-dev\n\
         \x20\x20--jsx-side-effects[=true|false]\n\
         \x20\x20--external:PATH\n\
         \x20\x20--alias:OLD=NEW\n\
         \x20\x20--packages=bundle|external\n\
         \x20\x20--loader=base64|binary|css|dataurl|default|empty|global-css|js|json|jsx|local-css|text|ts|tsx\n\
         \x20\x20--loader:.EXT=LOADER\n\
         \x20\x20--out-extension:.js=.mjs\n\
         \x20\x20--define:KEY=VALUE\n\
         \x20\x20--pure:CALL\n\
         \x20\x20--keep-names\n\
         \x20\x20--main-fields=FIELDS\n\
         \x20\x20--resolve-extensions=EXTENSIONS\n\
         \x20\x20--conditions=CONDITIONS\n\
         \x20\x20--drop:debugger\n\
         \x20\x20--drop:console\n\
         \x20\x20--drop-labels=LABELS\n\
         \x20\x20--ignore-annotations\n\
         \x20\x20--minify\n\
         \x20\x20--minify-whitespace\n\
         \x20\x20--minify-identifiers\n\
         \x20\x20--minify-syntax\n\
         \x20\x20--charset=ascii|utf8\n\
         \x20\x20--line-limit=N\n\
         \x20\x20--sourcefile=PATH\n\
         \x20\x20--banner=TEXT\n\
         \x20\x20--banner:js=TEXT\n\
         \x20\x20--banner:css=TEXT\n\
         \x20\x20--footer=TEXT\n\
         \x20\x20--footer:js=TEXT\n\
         \x20\x20--footer:css=TEXT\n\
         \x20\x20--version\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::{Loader, Output, parse_loader, run, run_with_stdin, run_with_stdin_and_node_paths};

    #[test]
    fn parses_loader_flags_and_file_extensions() {
        assert_eq!(parse_loader("tsx"), Ok(Loader::Tsx));
        assert_eq!(parse_loader("json"), Ok(Loader::Json));
        assert_eq!(parse_loader("dataurl"), Ok(Loader::DataUrl));
        assert!(parse_loader("wat").is_err());
        assert!(parse_loader("file").is_err());
        assert!(parse_loader("copy").is_err());
    }

    #[test]
    fn prints_help_and_version_without_input() {
        let Output::Text(help) = run(&["--help".into()]).expect("help succeeds") else {
            panic!("expected help text");
        };
        assert!(help.contains("Usage: esbuild"));
        let Output::Text(version) = run(&["--version".into()]).expect("version succeeds") else {
            panic!("expected version text");
        };
        assert_eq!(version, format!("{}\n", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn rejects_unknown_options() {
        assert!(run(&["--not-a-real-option".into()]).is_err());
        assert!(run(&["--loader=wat".into()]).is_err());
        assert!(run(&["--format=wat".into()]).is_err());
        assert!(run(&["--platform=wat".into()]).is_err());
        assert!(run(&["--packages=wat".into()]).is_err());
        assert!(run(&["--sourcemap=wat".into()]).is_err());
        assert!(run(&["--sources-content=wat".into()]).is_err());
        assert!(run(&["--tree-shaking=wat".into()]).is_err());
        assert!(run(&["--jsx=wat".into()]).is_err());
        assert!(run(&["--jsx-side-effects=wat".into()]).is_err());
        assert!(run(&["--alias:missing-value".into()]).is_err());
        assert!(run(&["--out-extension:.js".into()]).is_err());
        assert!(run(&["--drop-labels=".into()]).is_err());
        assert!(run(&["--drop-labels=DEV,".into()]).is_err());
        assert!(run(&["--pure:".into()]).is_err());
    }

    #[test]
    fn bundles_entry_files_to_stdout() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "console.log('cli bundle')").expect("write entry file");

        let Output::Code(output) = run(&["--bundle".into(), entry.to_string_lossy().into_owned()])
            .expect("bundle succeeds")
        else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(output.contains("console.log(\"cli bundle\");"));
        assert!(output.starts_with("(() => {\n"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bundles_standard_input() {
        let Output::Code(output) = run_with_stdin(
            &[
                "--bundle".into(),
                "--loader=ts".into(),
                "--sourcefile=virtual-entry.ts".into(),
            ],
            Some(b"const value: number = 42; console.log(value)"),
        )
        .expect("stdin bundle succeeds") else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(output.contains("const value = 42;"));
        assert!(output.contains("console.log(value);"));
        assert!(!output.contains(": number"));
    }

    #[test]
    fn resolves_packages_from_node_path() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-node-path-{unique}"));
        let global_modules = directory.join("global_modules");
        let package = global_modules.join("global-package");
        std::fs::create_dir_all(&package).expect("create global package directory");
        let entry = directory.join("entry.js");
        std::fs::write(
            &entry,
            "import {message} from 'global-package'; console.log(message)",
        )
        .expect("write entry file");
        std::fs::write(package.join("package.json"), r#"{"main":"index.js"}"#)
            .expect("write package metadata");
        std::fs::write(
            package.join("index.js"),
            "export const message = 'from NODE_PATH'",
        )
        .expect("write global package");

        let Output::Code(output) = run_with_stdin_and_node_paths(
            &["--bundle".into(), entry.to_string_lossy().into_owned()],
            None,
            vec![global_modules.to_string_lossy().into_owned()],
        )
        .expect("NODE_PATH bundle succeeds") else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(output.contains("\"from NODE_PATH\""), "{output}");
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn supports_advanced_entry_point_output_paths() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("esbuild-rs-cli-advanced-entry-{unique}"));
        let output_directory = directory.join("out");
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "console.log('advanced entry')").expect("write entry file");

        let Output::Text(output) = run(&[
            "--bundle".into(),
            format!("--outdir={}", output_directory.display()),
            format!("custom/application={}", entry.display()),
        ])
        .expect("bundle succeeds") else {
            panic!("expected file output");
        };
        assert!(output.is_empty());
        let generated = output_directory.join("custom/application.js");
        assert!(generated.is_file());
        assert!(
            std::fs::read_to_string(generated)
                .expect("read generated entry")
                .contains("console.log(\"advanced entry\")")
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn writes_multiple_non_bundled_entry_points() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-pass-through-{unique}"));
        let output_directory = directory.join("out");
        std::fs::create_dir_all(&directory).expect("create test directory");
        let first = directory.join("first.js");
        let second = directory.join("second.js");
        std::fs::write(
            &first,
            "import './dependency.js'; const unused = 1; console.log('first')",
        )
        .expect("write first entry");
        std::fs::write(&second, "console.log('second')").expect("write second entry");

        let Output::Text(output) = run(&[
            format!("--outdir={}", output_directory.display()),
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ])
        .expect("non-bundled build succeeds") else {
            panic!("expected file output");
        };
        assert!(output.is_empty());
        let first_output =
            std::fs::read_to_string(output_directory.join("first.js")).expect("read first output");
        assert!(
            first_output.contains("import \"./dependency.js\";"),
            "{first_output}"
        );
        assert!(first_output.contains("const unused = 1;"), "{first_output}");
        assert!(
            std::fs::read_to_string(output_directory.join("second.js"))
                .expect("read second output")
                .contains("console.log(\"second\")")
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn protects_input_files_from_output_overwrites() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-overwrite-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        let original = "console.log('original source')";
        std::fs::write(&entry, original).expect("write entry file");
        let arguments = vec![
            "--bundle".into(),
            format!("--outfile={}", entry.display()),
            entry.to_string_lossy().into_owned(),
        ];

        let Err(error) = run(&arguments) else {
            panic!("overwrite must be rejected");
        };
        assert!(error.contains("Refusing to overwrite input file"));
        assert_eq!(
            std::fs::read_to_string(&entry).expect("read protected input"),
            original
        );

        let mut allowed = arguments;
        allowed.insert(1, "--allow-overwrite".into());
        let Output::Text(output) = run(&allowed).expect("explicit overwrite succeeds") else {
            panic!("expected file output");
        };
        assert!(output.is_empty());
        let overwritten = std::fs::read_to_string(&entry).expect("read overwritten file");
        assert!(overwritten.contains("console.log(\"original source\")"));
        assert_ne!(overwritten, original);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn controls_tree_shaking_for_bundles() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-tree-shaking-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "const dead = 1; console.log('live')").expect("write entry file");

        for (setting, should_keep_dead) in [("true", false), ("false", true)] {
            let Output::Code(output) = run(&[
                "--bundle".into(),
                format!("--tree-shaking={setting}"),
                entry.to_string_lossy().into_owned(),
            ])
            .expect("bundle succeeds") else {
                panic!("expected bundled code");
            };
            let output = String::from_utf8(output).expect("bundle output is UTF-8");
            assert_eq!(output.contains("dead"), should_keep_dead);
            assert!(output.contains("console.log(\"live\")"));
        }

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn drops_configured_labels_for_bundles() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-drop-labels-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(
            &entry,
            "DEV: console.log('development'); PROD: console.log('production')",
        )
        .expect("write entry file");

        let Output::Code(output) = run(&[
            "--bundle".into(),
            "--drop-labels=DEV,TEST".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("bundle succeeds") else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(!output.contains("development"));
        assert!(!output.contains("DEV:"));
        assert!(output.contains("PROD:"));
        assert!(output.contains("console.log(\"production\")"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn drops_console_calls_for_standard_input() {
        let Output::Code(output) = run_with_stdin(
            &["--drop:console".into()],
            Some(b"console.log(sideEffect()); keep()"),
        )
        .expect("transform succeeds") else {
            panic!("expected transformed code");
        };
        assert_eq!(
            String::from_utf8(output).expect("transform output is UTF-8"),
            "keep();\n"
        );
    }

    #[test]
    fn configures_legal_comments_for_transforms() {
        let Output::Code(default_output) =
            run_with_stdin(&[], Some(b"//! default license\nkeep()"))
                .expect("default legal comments succeed")
        else {
            panic!("expected transformed code");
        };
        assert_eq!(
            String::from_utf8(default_output).expect("transform output is UTF-8"),
            "//! default license\nkeep();\n"
        );

        let Output::Code(output) = run_with_stdin(
            &["--legal-comments=eof".into()],
            Some(b"//! license\nkeep()"),
        )
        .expect("EOF legal comments succeed") else {
            panic!("expected transformed code");
        };
        assert_eq!(
            String::from_utf8(output).expect("transform output is UTF-8"),
            "keep();\n//! license\n"
        );

        for mode in ["linked", "external"] {
            let Err(error) = run_with_stdin(
                &[format!("--legal-comments={mode}")],
                Some(b"//! license\nkeep()"),
            ) else {
                panic!("separate legal comment output is unavailable for CLI transforms");
            };
            assert_eq!(
                error,
                "Cannot transform with linked or external legal comments"
            );
        }
    }

    #[test]
    fn generates_inline_source_maps_for_transforms() {
        for flag in ["--sourcemap", "--sourcemap=inline"] {
            let Output::Code(output) =
                run_with_stdin(&[flag.into(), "--sourcefile=stdin.js".into()], Some(b"1+2"))
                    .expect("inline source map succeeds")
            else {
                panic!("expected transformed code");
            };
            let output = String::from_utf8(output).expect("transform output is UTF-8");
            assert!(
                output.starts_with("1 + 2;\n//# sourceMappingURL=data:application/json;base64,"),
                "{output}"
            );
        }

        let Err(error) = run_with_stdin(&["--sourcemap".into()], Some(b"1+2")) else {
            panic!("source maps need a source file");
        };
        assert_eq!(
            error,
            "error: Must use \"sourcefile\" with \"sourcemap\" to set the original file name"
        );

        for mode in ["linked", "external", "both"] {
            let Err(error) = run_with_stdin(&[format!("--sourcemap={mode}")], Some(b"1+2")) else {
                panic!("CLI transforms only emit one output file");
            };
            assert_eq!(
                error,
                format!(
                    "Use \"--sourcemap\" instead of \"--sourcemap={mode}\" when transforming stdin"
                )
            );
        }
    }

    #[test]
    fn applies_raw_tsconfig_for_transforms() {
        let Output::Code(output) = run_with_stdin(
            &[
                "--loader=ts".into(),
                r#"--tsconfig-raw={"compilerOptions":{"alwaysStrict":true}}"#.into(),
            ],
            Some(b"console.log(123)"),
        )
        .expect("raw tsconfig transform succeeds") else {
            panic!("expected transformed code");
        };
        assert_eq!(
            String::from_utf8(output).expect("transform output is UTF-8"),
            "\"use strict\";\nconsole.log(123);\n"
        );
    }

    #[test]
    fn configures_jsx_side_effects_for_transforms() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("esbuild-rs-cli-jsx-side-effects-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.jsx");
        std::fs::write(&entry, "<Widget />").expect("write entry file");

        let Output::Code(output) = run(&[
            "--jsx-side-effects".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("transform succeeds") else {
            panic!("expected transformed code");
        };
        let output = String::from_utf8(output).expect("transform output is UTF-8");
        assert_eq!(output, "React.createElement(Widget, null);\n");

        let Output::Code(output) = run(&[
            "--jsx=preserve".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("transform succeeds") else {
            panic!("expected transformed code");
        };
        assert_eq!(
            String::from_utf8(output).expect("transform output is UTF-8"),
            "<Widget />;\n"
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn marks_configured_calls_as_pure() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-pure-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "factory(); keep(); console.log('live')").expect("write entry file");

        let Output::Code(output) = run(&[
            "--bundle".into(),
            "--pure:factory".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("bundle succeeds") else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(!output.contains("factory"));
        assert!(output.contains("keep();"));
        assert!(output.contains("console.log(\"live\")"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn keeps_names_for_transforms() {
        let Output::Code(output) = run_with_stdin(
            &["--keep-names".into()],
            Some(b"const PreservedArrow = () => {}; console.log(PreservedArrow.name)"),
        )
        .expect("transform succeeds") else {
            panic!("expected transformed code");
        };
        let output = String::from_utf8(output).expect("transform output is UTF-8");
        assert!(output.contains("Object.defineProperty"));
        assert!(output.contains("\"PreservedArrow\""));
        assert!(output.contains("console.log(PreservedArrow.name)"));
    }

    #[test]
    fn substitutes_defines_for_transforms() {
        let Output::Code(output) = run_with_stdin(
            &[
                "--define:process.env.NODE_ENV=\"production\"".into(),
                "--define:DEBUG=false".into(),
            ],
            Some(b"console.log(process.env.NODE_ENV, DEBUG)"),
        )
        .expect("transform succeeds") else {
            panic!("expected transformed code");
        };
        assert_eq!(
            String::from_utf8(output).expect("transform output is UTF-8"),
            "console.log(\"production\", false);\n"
        );
    }

    #[test]
    fn applies_platform_defaults_to_transforms() {
        let Output::Code(browser) = run_with_stdin(&[], Some(b"console.log(process.env.NODE_ENV)"))
            .expect("browser transform succeeds")
        else {
            panic!("expected transformed code");
        };
        assert!(
            String::from_utf8(browser)
                .expect("transform output is UTF-8")
                .contains("\"development\"")
        );

        let Output::Code(node) = run_with_stdin(
            &["--platform=node".into()],
            Some(b"console.log(process.env.NODE_ENV)"),
        )
        .expect("node transform succeeds") else {
            panic!("expected transformed code");
        };
        assert!(
            String::from_utf8(node)
                .expect("transform output is UTF-8")
                .contains("process.env.NODE_ENV")
        );
    }

    #[test]
    fn applies_css_bundle_banners_and_footers() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-css-banner-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.css");
        std::fs::write(&entry, ".entry { color: red }").expect("write CSS entry");
        let Output::Code(output) = run(&[
            "--bundle".into(),
            "--banner:css=/* before */".into(),
            "--footer:css=/* after */".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("CSS bundle succeeds") else {
            panic!("expected bundled CSS");
        };
        let output = String::from_utf8(output).expect("CSS output is UTF-8");
        assert!(output.starts_with("/* before */\n"));
        assert!(output.contains(".entry"));
        assert!(output.ends_with("/* after */\n"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bundles_with_external_package_flags() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-external-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(
            &entry,
            "import value from 'pkg/subpath'; console.log(value)",
        )
        .expect("write entry file");

        let Output::Code(output) = run(&[
            "--bundle".into(),
            "--format=esm".into(),
            "--external:pkg".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("bundle succeeds") else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(output.contains("from \"pkg/subpath\""));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn writes_linked_source_maps_for_bundles() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-sourcemap-{unique}"));
        let output_directory = directory.join("out");
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "console.log('cli source map')").expect("write entry file");

        let Output::Text(output) = run(&[
            "--bundle".into(),
            "--sourcemap".into(),
            "--source-root=https://cdn.example/source/".into(),
            "--sources-content=false".into(),
            format!("--outdir={}", output_directory.display()),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("bundle succeeds") else {
            panic!("expected file output");
        };
        assert!(output.is_empty());
        let javascript =
            std::fs::read_to_string(output_directory.join("entry.js")).expect("read JavaScript");
        assert!(javascript.contains("//# sourceMappingURL=entry.js.map"));
        let source_map =
            std::fs::read_to_string(output_directory.join("entry.js.map")).expect("read map");
        assert!(source_map.contains("\"version\": 3"));
        assert!(source_map.contains("\"sourceRoot\": \"https://cdn.example/source/\""));
        assert!(!source_map.contains("\"sourcesContent\""));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn writes_build_metafiles() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-metafile-{unique}"));
        let output_directory = directory.join("out");
        let metafile = directory.join("reports/meta.json");
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "console.log('cli metafile')").expect("write entry file");

        let Output::Text(output) = run(&[
            "--bundle".into(),
            format!("--outdir={}", output_directory.display()),
            format!("--metafile={}", metafile.display()),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("bundle succeeds") else {
            panic!("expected file output");
        };
        assert!(output.is_empty());
        let metadata = std::fs::read_to_string(&metafile).expect("read metafile");
        assert!(metadata.contains("\"inputs\": {"));
        assert!(metadata.contains("\"outputs\": {"));
        assert!(metadata.contains("entry.js\": {"));
        assert!(metadata.contains("out/entry.js\": {"));

        let non_bundle_output_directory = directory.join("non-bundle");
        let non_bundle_metafile = directory.join("reports/non-bundle.json");
        let Output::Text(output) = run(&[
            format!("--outdir={}", non_bundle_output_directory.display()),
            format!("--metafile={}", non_bundle_metafile.display()),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("non-bundled build succeeds") else {
            panic!("expected file output");
        };
        assert!(output.is_empty());
        let metadata =
            std::fs::read_to_string(&non_bundle_metafile).expect("read non-bundle metafile");
        assert!(metadata.contains("\"inputs\": {"));
        assert!(metadata.contains("\"outputs\": {"));
        assert!(metadata.contains("entry.js\": {"));
        assert!(metadata.contains("non-bundle/entry.js\": {"));

        assert!(
            run(&[
                "--bundle".into(),
                "--metafile=meta.json".into(),
                entry.to_string_lossy().into_owned(),
            ])
            .is_err()
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn analyzes_bundles() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-analyze-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "console.log('analyze')").expect("write entry file");

        for (flag, verbose) in [("--analyze", false), ("--analyze=verbose", true)] {
            let Output::WithStderr { output, stderr } = run(&[
                "--bundle".into(),
                flag.into(),
                entry.to_string_lossy().into_owned(),
            ])
            .expect("analysis succeeds") else {
                panic!("expected analysis output");
            };
            assert!(matches!(*output, Output::Code(_)));
            assert!(stderr.contains("100.0%"), "{stderr}");
            assert_eq!(stderr.contains('─'), verbose, "{stderr}");
            assert!(stderr.ends_with("\n\n"), "{stderr:?}");
        }

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bundles_with_extension_loader_overrides() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-loader-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(
            &entry,
            "import message from './message.custom'; console.log(message)",
        )
        .expect("write entry file");
        std::fs::write(directory.join("message.custom"), "cli custom loader")
            .expect("write custom input");

        let Output::Code(output) = run(&[
            "--bundle".into(),
            "--loader:.custom=text".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("bundle succeeds") else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(output.contains("\"cli custom loader\""));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bundles_with_define_substitutions() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-rs-cli-define-{unique}"));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let entry = directory.join("entry.js");
        std::fs::write(&entry, "console.log(DEBUG)").expect("write entry file");

        let Output::Code(output) = run(&[
            "--bundle".into(),
            "--define:DEBUG=false".into(),
            entry.to_string_lossy().into_owned(),
        ])
        .expect("bundle succeeds") else {
            panic!("expected bundled code");
        };
        let output = String::from_utf8(output).expect("bundle output is UTF-8");
        assert!(output.contains("console.log(false)"));
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }
}
