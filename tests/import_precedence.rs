use std::{
    io::Write,
    process::{Command, Stdio},
};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildStdin, Location, LogLevel, Message, TransformOptions, build,
    transform,
};
use serde_json::{Value, json};

// Captured independently with pkg/api from the revision in UPSTREAM.md.
// This checks diagnostics and output availability, not unrelated printer layout.
fn cases() -> Vec<Value> {
    serde_json::from_str(include_str!("import_precedence_go.json")).unwrap()
}

fn location(location: Option<&Location>) -> Value {
    location.map_or(Value::Null, |l| {
        json!({
            "file": l.file, "namespace": l.namespace, "line": l.line,
            "column": l.column, "length": l.length, "line_text": l.line_text,
            "suggestion": l.suggestion,
        })
    })
}

fn message(message: &Message) -> Value {
    json!({
        "id": message.id, "text": message.text,
        "location": location(message.location.as_ref()),
        "notes": message.notes.iter().map(|n| json!({
            "text": n.text, "location": location(n.location.as_ref()),
        })).collect::<Vec<_>>(),
        "plugin_name": message.plugin_name,
    })
}

#[test]
fn import_precedence_and_quoting_match_pinned_go_api_diagnostics() {
    for case in cases() {
        let mode = case["mode"].as_str().unwrap();
        let source = case["source"].as_str().unwrap();
        for (setting, level) in [
            ("default", LogLevel::Warning),
            ("silent", LogLevel::Silent),
            ("error", LogLevel::Error),
        ] {
            let id = if mode == "json" {
                "assert-type-json"
            } else {
                "assign-to-import"
            };
            let overrides = [(id.into(), level)].into();
            let (errors, warnings, has_code) = if mode == "transform" {
                let result = transform(
                    source,
                    TransformOptions {
                        sourcefile: "input.js".into(),
                        log_override: overrides,
                        ..TransformOptions::default()
                    },
                );
                (result.errors, result.warnings, !result.code.is_empty())
            } else {
                let result = build(BuildOptions {
                    stdin: Some(BuildStdin {
                        contents: source.into(),
                        sourcefile: "input.js".into(),
                        ..BuildStdin::default()
                    }),
                    bundle: true,
                    external: vec!["foo".into()],
                    format: BuildFormat::EsModule,
                    log_override: overrides,
                    ..BuildOptions::default()
                });
                (
                    result.errors,
                    result.warnings,
                    !result.output_files.is_empty(),
                )
            };
            let expected = &case["outcomes"][setting];
            assert_eq!(
                errors.len(),
                usize::try_from(expected["errors"].as_u64().unwrap()).unwrap(),
                "{} {setting}",
                case["name"]
            );
            assert_eq!(
                warnings.len(),
                usize::try_from(expected["warnings"].as_u64().unwrap()).unwrap(),
                "{} {setting}",
                case["name"]
            );
            assert_eq!(
                has_code,
                expected["has_code"].as_bool().unwrap(),
                "{} {setting}",
                case["name"]
            );
            if !errors.is_empty() || !warnings.is_empty() {
                let actual: Vec<_> = errors.iter().chain(&warnings).map(message).collect();
                assert_eq!(
                    json!(actual),
                    case["messages"],
                    "{} {setting}",
                    case["name"]
                );
            }
        }
    }
}

#[test]
fn import_precedence_cli_keeps_warning_ids_overrides_and_quoted_names() {
    for case in cases() {
        let source = case["source"].as_str().unwrap();
        let mode = case["mode"].as_str().unwrap();
        for setting in ["default", "silent", "error"] {
            let id = if mode == "json" {
                "assert-type-json"
            } else {
                "assign-to-import"
            };
            let mut command = Command::new(env!("CARGO_BIN_EXE_esbuild"));
            command.args([
                "--sourcefile=input.js",
                "--color=false",
                "--log-level=warning",
            ]);
            if mode != "transform" {
                command.args(["--bundle", "--external:foo", "--format=esm"]);
            }
            if setting != "default" {
                command.arg(format!("--log-override:{id}={setting}"));
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            let expected = &case["outcomes"][setting];
            assert_eq!(
                output.status.success(),
                expected["errors"] == 0,
                "{} {setting}",
                case["name"]
            );
            assert_eq!(
                !output.stdout.is_empty(),
                expected["has_code"].as_bool().unwrap(),
                "{} {setting}",
                case["name"]
            );
            let stderr = String::from_utf8(output.stderr).unwrap();
            if expected["errors"] == 0 && expected["warnings"] == 0 {
                assert!(stderr.is_empty(), "{} {setting}: {stderr}", case["name"]);
            } else {
                for diagnostic in case["messages"].as_array().unwrap() {
                    assert!(
                        stderr.contains(diagnostic["text"].as_str().unwrap()),
                        "{} {setting}: {stderr}",
                        case["name"]
                    );
                    for note in diagnostic["notes"].as_array().unwrap() {
                        assert!(
                            stderr.contains(note["text"].as_str().unwrap()),
                            "{} {setting}: {stderr}",
                            case["name"]
                        );
                    }
                    let warning_id = diagnostic["id"].as_str().unwrap();
                    if !warning_id.is_empty() {
                        assert!(stderr.contains(&format!("[{warning_id}]")));
                    }
                }
            }
        }
    }
}
