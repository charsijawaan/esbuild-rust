use std::{
    env, fs,
    io::{self, Read, Write},
};

use esbuild_rs::{
    api::{Loader, TransformOptions, transform},
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

fn run(arguments: &[String]) -> Result<Output, String> {
    let mut options = TransformOptions::default();
    let mut input_path = None;
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
        if input_path.replace(argument.clone()).is_some() {
            return Err("Only one input file can be transformed at a time".into());
        }
    }

    let input = if let Some(path) = input_path {
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
         \x20\x20--loader=base64|binary|css|dataurl|default|empty|global-css|js|json|jsx|local-css|text|ts|tsx\n\
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
    }
}
