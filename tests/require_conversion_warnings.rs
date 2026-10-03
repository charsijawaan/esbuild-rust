use std::{
    io::Write,
    process::{Command, Stdio},
};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildStdin, LogLevel, TransformOptions, build, transform,
};

const ID: &str = "unsupported-require-call";
const TEXT: &str = "Converting \"require\" to \"esm\" is currently not supported";

#[test]
fn esm_conversion_reports_require_calls_and_respects_context_and_overrides() {
    for (source, expected) in [
        ("const value = require('dependency');", 1),
        ("require(flag ? 'one' : 'two');", 2),
        ("try { require('dependency'); } catch {}", 0),
        (
            "let fn; try { fn = () => require('dependency'); } catch {}",
            1,
        ),
        ("const require = value => value; require('dependency');", 0),
        ("require.resolve('dependency');", 0),
        ("require?.('dependency');", 0),
        ("require(variable);", 0),
    ] {
        let result = transform(
            source,
            TransformOptions {
                sourcefile: "input.js".into(),
                format: BuildFormat::EsModule,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), expected, "{source}");
        for warning in &result.warnings {
            assert_eq!(warning.id, ID);
            assert_eq!(warning.text, TEXT);
            let location = warning.location.as_ref().unwrap();
            assert_eq!(location.file, "input.js");
            assert_eq!(location.length, 7);
            assert_eq!(
                &source[location.column..location.column + location.length],
                "require"
            );
        }
    }
    let source = "require('dependency');";
    for format in [
        BuildFormat::Default,
        BuildFormat::CommonJs,
        BuildFormat::Iife,
    ] {
        assert!(
            transform(
                source,
                TransformOptions {
                    format,
                    ..TransformOptions::default()
                }
            )
            .warnings
            .is_empty()
        );
    }
    for bundle in [false, true] {
        let result = build(BuildOptions {
            stdin: Some(BuildStdin {
                contents: source.into(),
                ..BuildStdin::default()
            }),
            bundle,
            external: if bundle { vec!["*".into()] } else { Vec::new() },
            format: BuildFormat::EsModule,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), usize::from(!bundle));
    }
    for (level, errors, warnings) in [(LogLevel::Silent, 0, 0), (LogLevel::Error, 1, 0)] {
        let result = transform(
            source,
            TransformOptions {
                format: BuildFormat::EsModule,
                log_override: std::collections::HashMap::from([(ID.into(), level)]),
                ..TransformOptions::default()
            },
        );
        assert_eq!(result.errors.len(), errors);
        assert_eq!(result.warnings.len(), warnings);
    }
}

#[test]
fn cli_reports_and_filters_unsupported_require_conversion() {
    for (argument, success, warning) in [
        ("--log-level=warning", true, true),
        (
            "--log-override:unsupported-require-call=silent",
            true,
            false,
        ),
        ("--log-override:unsupported-require-call=error", false, true),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(["--format=esm", argument])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"require('dependency');")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.success(), success);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.contains(ID), warning);
        assert_eq!(stderr.contains(TEXT), warning);
    }
}
