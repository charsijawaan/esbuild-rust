use std::io::Write;
use std::process::{Command, Stdio};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{
    BuildOptions, BuildStdin, Engine, EngineName, Loader, Message, MessageKind, Target,
    TransformOptions, build, context, transform,
};
use serde_json::{Value, json};

const NOTE: &str = "In this environment, the \"Function.prototype.name\" property is not configurable \
                    and assigning to it will throw an error. Either use a newer target environment \
                    or disable the \"keep names\" setting.";

fn chrome(version: &str) -> Vec<Engine> {
    vec![Engine {
        name: EngineName::Chrome,
        version: version.into(),
    }]
}

fn options(engines: Vec<Engine>, supported: &[(&str, bool)]) -> TransformOptions {
    TransformOptions {
        keep_names: true,
        engines,
        supported: supported
            .iter()
            .map(|(name, value)| ((*name).into(), *value))
            .collect(),
        ..TransformOptions::default()
    }
}

fn request(mode: &str, input: &str, options: &TransformOptions) -> Value {
    json!({
        "mode": mode,
        "input": input,
        "keepNames": options.keep_names,
        "target": options.target as u8,
        "engines": options.engines.iter().map(|engine| json!({
            "Name": engine.name as u8,
            "Version": engine.version,
        })).collect::<Vec<_>>(),
        "supported": options.supported,
        "loader": options.loader as u16,
        "minifyIdentifiers": options.minify_identifiers,
    })
}

fn messages(messages: &[Message]) -> Value {
    json!(
        messages
            .iter()
            .map(|message| {
                assert!(message.location.is_none(), "{message:?}");
                assert!(message.detail.is_none(), "{message:?}");
                json!({
                    "ID": message.id,
                    "PluginName": message.plugin_name,
                    "Text": message.text,
                    "Location": null,
                    "Notes": message.notes.iter().map(|note| {
                        assert!(note.location.is_none());
                        json!({"Text": note.text, "Location": null})
                    }).collect::<Vec<_>>(),
                    "Detail": null,
                })
            })
            .collect::<Vec<_>>()
    )
}

// Set this to the small native API probe built against the unchanged pinned Go
// checkout. Ordinary regression runs do not need Go or that checkout.
fn compare_go(request: &Value, errors: &[Message], warnings: &[Message], outputs: &[Vec<u8>]) {
    let Some(executable) = std::env::var_os("ESBUILD_RS_KEEP_NAMES_GO") else {
        return;
    };
    let mut reference = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run pinned Go native API probe");
    reference
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(request).unwrap())
        .unwrap();
    let reference = reference.wait_with_output().unwrap();
    assert!(reference.status.success(), "{reference:?}");
    let mut reference: Value = serde_json::from_slice(&reference.stdout).unwrap();
    for name in ["errors", "warnings"] {
        if reference[name].is_null() {
            reference[name] = json!([]);
        }
        for message in reference[name].as_array_mut().unwrap() {
            if message["Notes"].is_null() {
                message["Notes"] = json!([]);
            }
        }
    }
    assert_eq!(messages(errors), reference["errors"], "{request}");
    assert_eq!(messages(warnings), reference["warnings"], "{request}");
    let expected = reference["outputs"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|value| {
            if value.is_null() {
                Vec::new()
            } else {
                STANDARD.decode(value.as_str().unwrap()).unwrap()
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs, expected, "{request}");
}

fn assert_error(errors: &[Message], environment: &str) {
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].kind, MessageKind::Error);
    assert_eq!(
        errors[0].text,
        format!(
            "The \"keep names\" setting cannot be used with the configured target environment{environment}"
        )
    );
    assert!(errors[0].id.is_empty());
    assert!(errors[0].plugin_name.is_empty());
    assert!(errors[0].location.is_none());
    assert!(errors[0].detail.is_none());
    assert_eq!(errors[0].notes.len(), 1);
    assert_eq!(errors[0].notes[0].text, NOTE);
    assert!(errors[0].notes[0].location.is_none());
}

#[test]
fn rejects_unsupported_keep_names_in_build_transform_and_context() {
    let cases = [
        (options(chrome("36"), &[]), " (\"chrome36\")"),
        (options(chrome("42"), &[]), " (\"chrome42\")"),
        (
            options(chrome("46"), &[("function-name-configurable", false)]),
            " (\"chrome46\" + 1 override)",
        ),
        (
            TransformOptions {
                target: Target::Es5,
                ..options(Vec::new(), &[])
            },
            " (\"es5\")",
        ),
        (
            options(Vec::new(), &[("function-name-configurable", false)]),
            "",
        ),
        (
            TransformOptions {
                target: Target::Es2015,
                ..options(
                    chrome("36"),
                    &[
                        ("function-name-configurable", false),
                        ("optional-chain", true),
                        ("nesting", false), // CSS overrides do not count here.
                    ],
                )
            },
            " (\"chrome36\", \"es2015\" + 2 overrides)",
        ),
    ];
    for (options, environment) in cases {
        let transformed = transform("", options.clone());
        assert_error(&transformed.errors, environment);
        assert!(transformed.warnings.is_empty());
        assert!(transformed.code.is_empty());
        assert!(transformed.map.is_empty());
        compare_go(
            &request("transform", "", &options),
            &transformed.errors,
            &transformed.warnings,
            &[transformed.code],
        );
        let build_options = BuildOptions {
            stdin: Some(BuildStdin::default()),
            outfile: "out.js".into(),
            keep_names: options.keep_names,
            target: options.target,
            engines: options.engines.clone(),
            supported: options.supported.clone(),
            tsconfig_raw: "{}".into(),
            ..BuildOptions::default()
        };
        let built = build(build_options.clone());
        assert_error(&built.errors, environment);
        assert!(built.warnings.is_empty());
        assert!(built.output_files.is_empty());
        compare_go(
            &request("build", "", &options),
            &built.errors,
            &built.warnings,
            &[],
        );
        let error = match context(build_options) {
            Ok(context) => {
                context.dispose();
                panic!("context creation accepted unsupported keepNames")
            }
            Err(error) => error,
        };
        assert_error(&error.errors, environment);
        compare_go(&request("context", "", &options), &error.errors, &[], &[]);
    }
}

#[test]
fn validates_keep_names_for_direct_non_javascript_transform_loaders() {
    for loader in [Loader::Css, Loader::Json, Loader::Text, Loader::Empty] {
        let options = TransformOptions {
            loader,
            ..options(chrome("36"), &[])
        };
        let result = transform("", options.clone());
        assert_error(&result.errors, " (\"chrome36\")");
        assert!(result.warnings.is_empty());
        assert!(result.code.is_empty());
        compare_go(
            &request("transform", "", &options),
            &result.errors,
            &result.warnings,
            &[result.code],
        );
    }
}

#[test]
fn accepts_supported_targets_overrides_and_disabled_keep_names() {
    let cases = [
        options(chrome("43"), &[]),
        options(chrome("46"), &[]),
        options(chrome("36"), &[("function-name-configurable", true)]),
        TransformOptions {
            keep_names: false,
            ..options(chrome("36"), &[("function-name-configurable", false)])
        },
    ];
    for options in cases {
        let result = transform("", options.clone());
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty());
        assert!(result.code.is_empty());
        compare_go(
            &request("transform", "", &options),
            &[],
            &[],
            &[result.code],
        );
        let result = build(BuildOptions {
            stdin: Some(BuildStdin::default()),
            outfile: "out.js".into(),
            keep_names: options.keep_names,
            target: options.target,
            engines: options.engines.clone(),
            supported: options.supported.clone(),
            tsconfig_raw: "{}".into(),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty());
        assert_eq!(result.output_files.len(), 1);
        compare_go(
            &request("build", "", &options),
            &[],
            &[],
            &[result.output_files[0].contents.clone()],
        );
    }
}

#[test]
fn supported_name_override_preserves_generated_helper_lowering() {
    let source = "var LongFunctionName = function() {}; console.log(LongFunctionName.name);";
    let options = TransformOptions {
        minify_identifiers: true,
        ..options(chrome("36"), &[("function-name-configurable", true)])
    };
    let result = transform(source, options.clone());
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty());
    let code = String::from_utf8(result.code.clone()).unwrap();
    assert!(code.contains("Object.defineProperty"), "{code}");
    assert!(code.contains("configurable: true"), "{code}");
    assert!(code.contains("\"LongFunctionName\""), "{code}");
    assert!(!code.contains("=>"), "{code}");
    compare_go(
        &request("transform", source, &options),
        &[],
        &[],
        &[result.code],
    );
    if std::env::var_os("ESBUILD_RS_KEEP_NAMES_GO").is_some() {
        let output = Command::new("node").arg("-e").arg(code).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"LongFunctionName\n");
    }
}
