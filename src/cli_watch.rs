use std::{
    fs,
    io::{self, IsTerminal, Read},
    path::Path,
    sync::Arc,
};

use esbuild_rs::{
    api::{
        AbsPaths, AnalyzeMetafileOptions, BuildOptions, BuildResult, MessageKind, OnEndResult,
        Plugin, WatchOptions, analyze_metafile, context,
    },
    internal::{
        cli_helpers,
        fs::{RealFsOptions, real_fs},
        logger::{LogLevel, UseColor, print_text_with_color},
    },
};

use super::{
    AnalyzeMode, cli_color, cli_log_level, cli_message_summary, format_cli_message_details,
    format_cli_messages,
};

fn write_metafile(path: &str, contents: &str) -> io::Result<()> {
    let path = Path::new(path);
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

fn print_status(color: UseColor, text: &str) {
    let _ = print_text_with_color(&mut io::stderr(), color, |colors| {
        format!("{}{text}{}\n", colors.dim, colors.reset)
    });
}

fn finish_build(result: &BuildResult, arguments: &[String], metafile: &str, analyze: AnalyzeMode) {
    let mut stderr = format_cli_message_details(arguments, &result.warnings, MessageKind::Warning);
    stderr.push_str(&format_cli_message_details(
        arguments,
        &result.errors,
        MessageKind::Error,
    ));
    if result.errors.is_empty() {
        if !metafile.is_empty() {
            if let Err(error) = write_metafile(metafile, &result.metafile) {
                stderr.push_str(&format_cli_messages(
                    arguments,
                    &[esbuild_rs::api::Message {
                        text: format!("Could not write {metafile:?}: {error}"),
                        ..esbuild_rs::api::Message::default()
                    }],
                    MessageKind::Error,
                ));
                stderr.push('\n');
            }
        }
        if analyze != AnalyzeMode::Disabled {
            stderr.push_str(&analyze_metafile(
                &result.metafile,
                AnalyzeMetafileOptions {
                    color: cli_color(arguments),
                    verbose: analyze == AnalyzeMode::Verbose,
                },
            ));
            stderr.push('\n');
        }
    }
    let errors = cli_message_summary(arguments, &result.errors, MessageKind::Error);
    let warnings = cli_message_summary(arguments, &result.warnings, MessageKind::Warning);
    let summary = [
        (!result.warnings.is_empty()).then_some(warnings),
        (!result.errors.is_empty()).then_some(errors),
    ]
    .into_iter()
    .flatten()
    .filter(|text| !text.is_empty())
    .collect::<Vec<_>>()
    .join(" and ");
    if !summary.is_empty() {
        stderr.push_str(&summary);
        stderr.push('\n');
    }
    eprint!("{stderr}");
}

pub(super) fn run(
    mut options: BuildOptions,
    arguments: &[String],
    forever: bool,
    delay: i64,
    metafile: String,
    analyze: AnalyzeMode,
) -> Result<(), String> {
    options.write = true;
    let arguments = arguments.to_vec();
    let callback_arguments = arguments.clone();
    let log_status = cli_log_level(&arguments) <= LogLevel::Info;
    let color = if cli_color(&arguments) {
        UseColor::Always
    } else {
        UseColor::Never
    };
    let absolute = options.abs_paths.contains(AbsPaths::LOG);
    let file_system = real_fs(RealFsOptions {
        abs_working_dir: options.abs_working_dir.clone(),
        ..RealFsOptions::default()
    })
    .map_err(|error| error.to_string())?;
    options
        .plugins
        .push(Plugin::new("PostBuildActions", move |build| {
            let arguments = callback_arguments.clone();
            let metafile = metafile.clone();
            build.on_end(move |result| {
                finish_build(result, &arguments, &metafile, analyze);
                Ok(OnEndResult::default())
            });
            Ok(())
        }));
    let build_context = context(options)
        .map_err(|error| format_cli_messages(&arguments, &error.errors, MessageKind::Error))?;
    cli_helpers::watch_context(
        &build_context,
        WatchOptions { delay },
        Arc::new(move |path, finished| {
            if !log_status {
                return;
            }
            let text = match (path, finished) {
                (None, _) => {
                    let delay = if delay > 0 {
                        format!(" with a {delay}ms delay")
                    } else {
                        String::new()
                    };
                    format!("[watch] build finished, watching for changes{delay}...")
                }
                (Some(_), true) => "[watch] build finished".into(),
                (Some(path), false) => {
                    let path = if absolute {
                        path.into()
                    } else {
                        file_system
                            .rel(file_system.cwd(), path)
                            .unwrap_or_else(|| path.into())
                            .replace('\\', "/")
                    };
                    format!("[watch] build started (change: {path:?})")
                }
            };
            print_status(color, &text);
        }),
    )
    .map_err(|error| error.to_string())?;
    if forever || io::stdin().is_terminal() {
        loop {
            std::thread::park();
        }
    }
    let mut buffer = [0_u8; 512];
    let result = loop {
        match io::stdin().read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(4)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => break Err(format!("Could not read stdin: {error}")),
        }
    };
    if log_status {
        print_status(
            color,
            "[watch] stopped automatically because stdin was closed (use \"--watch=forever\" to keep watching even after stdin is closed)",
        );
    }
    build_context.dispose();
    result
}
