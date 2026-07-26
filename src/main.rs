use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
};

use esbuild_rs::{
    api::{
        BuildFormat, BuildLegalComments, BuildOptions, BuildPlatform, BuildSourceMap, Loader,
        Packages, TransformOptions, build, transform,
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
    let mut format = BuildFormat::Iife;
    let mut platform = BuildPlatform::Browser;
    let mut global_name = String::new();
    let mut splitting = false;
    let mut sourcemap = BuildSourceMap::None;
    let mut legal_comments = BuildLegalComments::Inline;
    let mut external = Vec::new();
    let mut packages = Packages::Bundle;
    let mut build_loaders = HashMap::new();
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
        if let Some(value) = argument.strip_prefix("--external:") {
            external.push(value.into());
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
        let result = build(BuildOptions {
            entry_points: input_paths,
            outdir: outdir.clone(),
            outfile: outfile.clone(),
            outbase,
            format,
            platform,
            global_name,
            sourcemap,
            legal_comments,
            splitting,
            minify_whitespace: options.minify_whitespace,
            minify_identifiers: options.minify_identifiers,
            minify_syntax: options.minify_syntax,
            ascii_only: options.ascii_only,
            banner: options.banner,
            footer: options.footer,
            external,
            packages,
            loader: build_loaders,
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
        return Ok(Output::Text(String::new()));
    }

    if sourcemap != BuildSourceMap::None {
        return Err("\"--sourcemap\" without \"--bundle\" is not implemented yet".into());
    }
    if input_paths.len() > 1 {
        return Err("Only one input file can be transformed at a time".into());
    }
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
         \x20\x20--format=iife|cjs|esm\n\
         \x20\x20--platform=browser|node|neutral\n\
         \x20\x20--global-name=NAME\n\
         \x20\x20--splitting\n\
         \x20\x20--sourcemap[=linked|external|inline|both]\n\
         \x20\x20--legal-comments=none|inline|eof|linked|external\n\
         \x20\x20--external:PATH\n\
         \x20\x20--packages=bundle|external\n\
         \x20\x20--loader=base64|binary|css|dataurl|default|empty|global-css|js|json|jsx|local-css|text|ts|tsx\n\
         \x20\x20--loader:.EXT=LOADER\n\
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
}
