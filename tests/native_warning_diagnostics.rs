//! Native warning fields and overrides, independent of plugins and contexts.

use esbuild_rs::api::{
    BuildOptions, BuildStdin, LogLevel, Message, TransformOptions, build, transform,
};
use std::{
    collections::HashMap,
    io::Write,
    process::{Command, Stdio},
};

const TEXT: &str = "\"process.env.NODE_ENV\" is defined as an identifier instead of a string (surround \"production\" with quotes to get a string)";

fn define(value: &str) -> HashMap<String, String> {
    HashMap::from([("process.env.NODE_ENV".into(), value.into())])
}

fn check_go_warning(message: &Message) {
    assert_eq!(message.id, "suspicious-define");
    assert_eq!(message.text, TEXT);
    assert!(message.plugin_name.is_empty());
    assert!(message.notes.is_empty());
    assert!(message.detail.is_none());
    let location = message.location.as_ref().unwrap();
    assert_eq!(location.file, "<go>");
    assert!(location.namespace.is_empty());
    assert_eq!(
        (location.line, location.column, location.length),
        (1, 50, 12)
    );
    assert_eq!(
        location.line_text,
        "Define: map[string]string{\"process.env.NODE_ENV\": \"production\"}"
    );
    assert_eq!(location.suggestion, "\"\\\"production\\\"\"");
}

#[test]
fn suspicious_define_uses_go_public_api_warning_fields_once() {
    let transformed = transform(
        "",
        TransformOptions {
            define: define("production"),
            ..TransformOptions::default()
        },
    );
    assert!(transformed.errors.is_empty(), "{:?}", transformed.errors);
    assert_eq!(transformed.warnings.len(), 1);
    check_go_warning(&transformed.warnings[0]);
    let built = build(BuildOptions {
        define: define("production"),
        ..BuildOptions::default()
    });
    assert!(built.errors.is_empty(), "{:?}", built.errors);
    assert_eq!(built.warnings.len(), 1);
    check_go_warning(&built.warnings[0]);
}

#[test]
fn suspicious_define_requires_one_entity_part_and_exact_normalized_key() {
    for (value, expected) in [
        ("production", 1),
        ("development", 1),
        ("undefined", 1),
        ("Infinity", 1),
        ("NaN", 1),
        ("this", 1),
        ("env.production", 0),
        ("\"production\"", 0),
        ("null", 1),
        ("[1]", 0),
    ] {
        let result = transform(
            "",
            TransformOptions {
                define: define(value),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{value}: {:?}", result.errors);
        assert_eq!(result.warnings.len(), expected, "{value}");
    }
    for (key, expected) in [
        ("process.env.NODE_ENV", 1),
        ("process['env'].NODE_ENV", 1),
        ("process.env.OTHER", 0),
        ("other.env.NODE_ENV", 0),
    ] {
        let result = transform(
            "",
            TransformOptions {
                define: HashMap::from([(key.into(), "production".into())]),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{key}: {:?}", result.errors);
        assert_eq!(result.warnings.len(), expected, "{key}");
    }
}

#[test]
fn suspicious_define_log_overrides_promote_suppress_and_preserve_detail() {
    for (level, errors, warnings) in [
        (LogLevel::Warning, 0, 1),
        (LogLevel::Error, 1, 0),
        (LogLevel::Silent, 0, 0),
        (LogLevel::Debug, 0, 0),
    ] {
        let log_override = HashMap::from([("suspicious-define".into(), level)]);
        let transformed = transform(
            "",
            TransformOptions {
                define: define("production"),
                log_override: log_override.clone(),
                ..TransformOptions::default()
            },
        );
        let built = build(BuildOptions {
            define: define("production"),
            log_override,
            ..BuildOptions::default()
        });
        for (actual_errors, actual_warnings) in [
            (&transformed.errors, &transformed.warnings),
            (&built.errors, &built.warnings),
        ] {
            assert_eq!(actual_errors.len(), errors);
            assert_eq!(actual_warnings.len(), warnings);
            for message in actual_errors.iter().chain(actual_warnings.iter()) {
                check_go_warning(message);
            }
        }
    }
}

#[test]
fn prerequisite_typeof_warning_has_original_undefined_detail_and_exact_range() {
    let source = "typeof x == \"null\"";
    let transformed = transform(source, TransformOptions::default());
    let built = build(BuildOptions {
        stdin: Some(BuildStdin {
            contents: source.into(),
            ..BuildStdin::default()
        }),
        ..BuildOptions::default()
    });
    assert!(transformed.errors.is_empty());
    assert!(built.errors.is_empty());
    for warnings in [&transformed.warnings, &built.warnings] {
        assert_eq!(warnings.len(), 1);
        let message = &warnings[0];
        assert_eq!(message.id, "impossible-typeof");
        assert_eq!(
            message.text,
            "The \"typeof\" operator will never evaluate to \"null\""
        );
        assert!(message.plugin_name.is_empty());
        assert!(message.detail.is_none());
        let location = message.location.as_ref().unwrap();
        assert_eq!(location.file, "<stdin>");
        assert!(location.namespace.is_empty());
        assert_eq!(
            (location.line, location.column, location.length),
            (1, 12, 6)
        );
        assert_eq!(location.line_text, source);
        assert!(location.suggestion.is_empty());
        assert_eq!(message.notes.len(), 1);
        assert!(message.notes[0].location.is_none());
        assert_eq!(
            message.notes[0].text,
            "The expression \"typeof x\" actually evaluates to \"object\" in JavaScript, not \"null\". You need to use \"x === null\" to test for null."
        );
    }
}

#[test]
fn suspicious_define_cli_location_and_quoting_match_go() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .args([
            "--define:process.env.NODE_ENV=production",
            "--color=false",
            "--log-level=warning",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "▲ [WARNING] {TEXT} [suspicious-define]\n\n    <cli>:1:30:\n      1 │ --define:process.env.NODE_ENV=production\n        │                               ~~~~~~~~~~\n        ╵                               \\\"production\\\"\n\n"
        )
    );
}
