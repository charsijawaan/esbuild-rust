use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
};

use esbuild_rs::{
    api::{
        BuildFormat, BuildJsx, BuildLegalComments, BuildOptions, BuildPlatform, BuildSourceMap,
        BuildTreeShaking, Loader, Packages, TransformOptions, build, transform,
    },
    internal::cli_helpers,
};

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match run(&arguments) {
        Ok(Output::Text(text)) => {
            print!("{text}");
        }
        Ok(Output::Code(code)) => {
            if let Err(error) = io::stdout().write_all(&code) {
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
}

#[allow(clippy::too_many_lines)]
fn run(arguments: &[String]) -> Result<Output, String> {
    let mut options = TransformOptions::default();
    let mut input_paths = Vec::new();
    let mut bundle = false;
    let mut outdir = String::new();
    let mut outfile = String::new();
    let mut outbase = String::new();
    let mut tsconfig = String::new();
    let mut metafile_path = String::new();
    let mut format = BuildFormat::Iife;
    let mut platform = BuildPlatform::Browser;
    let mut global_name = String::new();
    let mut public_path = String::new();
    let mut entry_names = String::new();
    let mut chunk_names = String::new();
    let mut asset_names = String::new();
    let mut splitting = false;
    let mut sourcemap = BuildSourceMap::None;
    let mut legal_comments = BuildLegalComments::Inline;
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
        if argument == "--splitting" {
            splitting = true;
            continue;
        }
        if argument == "--drop:debugger" {
            options.drop_debugger = true;
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
            continue;
        }
        if let Some(value) = argument.strip_prefix("--sourcemap=") {
            sourcemap = match value {
                "linked" => BuildSourceMap::Linked,
                "external" => BuildSourceMap::External,
                "inline" => BuildSourceMap::Inline,
                "both" => BuildSourceMap::InlineAndExternal,
                _ => return Err(format!("Invalid source map setting {value:?}")),
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
        if let Some(banner) = argument.strip_prefix("--banner=") {
            options.banner = banner.into();
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

    if bundle {
        if input_paths.is_empty() {
            return Err("Bundling from stdin is not implemented yet".into());
        }
        if !outdir.is_empty() && !outfile.is_empty() {
            return Err("Cannot use both \"--outfile\" and \"--outdir\"".into());
        }
        if options.loader != Loader::None {
            return Err("Use \"--loader:.ext=loader\" when bundling".into());
        }
        if !metafile_path.is_empty() && outdir.is_empty() && outfile.is_empty() {
            return Err("Cannot use \"--metafile\" without an output path".into());
        }
        let result = build(BuildOptions {
            entry_points: input_paths,
            outdir: outdir.clone(),
            outfile: outfile.clone(),
            outbase,
            tsconfig,
            metafile: !metafile_path.is_empty(),
            format,
            platform,
            global_name,
            public_path,
            entry_names,
            chunk_names,
            asset_names,
            sourcemap,
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
            minify_whitespace: options.minify_whitespace,
            minify_identifiers: options.minify_identifiers,
            minify_syntax: options.minify_syntax,
            ascii_only: options.ascii_only,
            drop_debugger: options.drop_debugger,
            drop_labels: options.drop_labels,
            ignore_annotations: options.ignore_annotations,
            banner: options.banner,
            footer: options.footer,
            external,
            alias: aliases,
            packages,
            loader: build_loaders,
            out_extension: out_extensions,
            define: defines,
            pure,
            main_fields,
            resolve_extensions,
            conditions,
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
        if outdir.is_empty() && outfile.is_empty() {
            let [output] = result.output_files.as_slice() else {
                return Err("Must use \"--outdir\" when there are multiple output files".into());
            };
            return Ok(Output::Code(output.contents.clone()));
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
        return Ok(Output::Text(String::new()));
    }

    if !defines.is_empty() {
        return Err("\"--define\" without \"--bundle\" is not implemented yet".into());
    }
    if !metafile_path.is_empty() {
        return Err("\"--metafile\" without \"--bundle\" is not implemented yet".into());
    }
    if sourcemap != BuildSourceMap::None {
        return Err("\"--sourcemap\" without \"--bundle\" is not implemented yet".into());
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
    options.pure = pure;
    let input = if let Some(path) = input_paths.pop() {
        if options.sourcefile.is_empty() {
            options.sourcefile.clone_from(&path);
        }
        if options.loader == Loader::None {
            options.loader = Loader::Default;
        }
        fs::read(&path).map_err(|error| format!("Could not read {path:?}: {error}"))?
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
         \x20\x20--outdir=DIR\n\
         \x20\x20--outfile=FILE\n\
         \x20\x20--outbase=DIR\n\
         \x20\x20--tsconfig=FILE\n\
         \x20\x20--metafile=FILE\n\
         \x20\x20--format=iife|cjs|esm\n\
         \x20\x20--platform=browser|node|neutral\n\
         \x20\x20--global-name=NAME\n\
         \x20\x20--public-path=PATH\n\
         \x20\x20--entry-names=TEMPLATE\n\
         \x20\x20--chunk-names=TEMPLATE\n\
         \x20\x20--asset-names=TEMPLATE\n\
         \x20\x20--splitting\n\
         \x20\x20--sourcemap[=linked|external|inline|both]\n\
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
         \x20\x20--main-fields=FIELDS\n\
         \x20\x20--resolve-extensions=EXTENSIONS\n\
         \x20\x20--conditions=CONDITIONS\n\
         \x20\x20--drop:debugger\n\
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
         \x20\x20--footer=TEXT\n\
         \x20\x20--version\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::{Loader, Output, parse_loader, run};

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
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn writes_bundle_metafiles() {
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
