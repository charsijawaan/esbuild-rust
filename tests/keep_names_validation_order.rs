//! Combined option diagnostics, with an optional unmodified pinned Go reference.

use esbuild_rs::api::{
    self, BuildLegalComments, BuildOptions, BuildSourceMap, BuildStdin, Engine, EngineName, Loader,
    Message, MessageKind, TransformOptions,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    process::{Command, Stdio},
};

const DEFINE: &str = "Invalid define value (must be an entity name or JS literal): bad + value";
const MAP: &str = "Cannot transform with linked source maps";
const LEGAL: &str = "Cannot transform with linked legal comments";
const NOTE: &str = "In this environment, the \"Function.prototype.name\" property is not configurable and assigning to it will throw an error. Either use a newer target environment or disable the \"keep names\" setting.";

fn options() -> TransformOptions {
    TransformOptions {
        keep_names: true,
        sourcefile: "input.js".into(),
        engines: vec![Engine {
            name: EngineName::Chrome,
            version: "36".into(),
        }],
        ..TransformOptions::default()
    }
}

fn define() -> HashMap<String, String> {
    HashMap::from([("BAD".into(), "bad + value".into())])
}

fn check_fields(errors: &[Message]) {
    for message in errors {
        assert_eq!(message.kind, MessageKind::Error);
        assert!(message.id.is_empty());
        assert!(message.plugin_name.is_empty());
        assert!(message.location.is_none());
        assert!(message.detail.is_none());
        assert!(message.notes.iter().all(|note| note.location.is_none()));
        if message.text.starts_with("The \"keep names\" setting") {
            assert_eq!(message.notes.len(), 1);
            assert_eq!(message.notes[0].text, NOTE);
        }
    }
}

fn keep(environment: &str) -> String {
    format!(
        "The \"keep names\" setting cannot be used with the configured target environment{environment}"
    )
}

fn compare_go(mode: &str, options: &TransformOptions, errors: &[Message]) {
    let Some(executable) = std::env::var_os("ESBUILD_RS_KEEP_NAMES_ORDER_GO") else {
        return;
    };
    let request = json!({
        "Mode": mode, "Input": "", "KeepNames": options.keep_names,
        "Supported": options.supported.get("function-name-configurable") == Some(&true),
        "Unsupported": options.supported.get("function-name-configurable") == Some(&false),
        "InvalidFeature": options.supported.contains_key("UNKNOWN"),
        "Define": !options.define.is_empty(), "Factory": !options.jsx_factory.is_empty(),
        "Fragment": !options.jsx_fragment.is_empty(), "Mangle": !options.mangle_props.is_empty(),
        "InvalidVersion": options.engines[0].version == "invalid",
        "LinkedMap": options.sourcemap == BuildSourceMap::Linked,
        "LinkedLegal": options.legal_comments == BuildLegalComments::Linked,
        "DefaultSourcefile": options.sourcefile.is_empty(),
        "Loader": match options.loader {
            Loader::Css => "css", Loader::Json => "json", Loader::Text => "text",
            Loader::Empty => "empty", _ => "js",
        },
    });
    let mut child = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut reference: Value = serde_json::from_slice(&output.stdout).unwrap();
    if reference["errors"].is_null() {
        reference["errors"] = json!([]);
    }
    for message in reference["errors"].as_array_mut().unwrap() {
        if message["Notes"].is_null() {
            message["Notes"] = json!([]);
        }
    }
    assert!(reference["warnings"].is_null());
    let actual = json!(errors.iter().map(|message| json!({
        "ID": message.id, "PluginName": message.plugin_name, "Text": message.text,
        "Location": null, "Detail": message.detail.as_ref().map(|_| "<opaque>"),
        "Notes": message.notes.iter().map(|note| json!({"Text": note.text, "Location": null})).collect::<Vec<_>>(),
    })).collect::<Vec<_>>());
    // Full text comparisons intentionally retain inherited JSX/regex failures.
    assert_eq!(actual, reference["errors"], "{request}");
}

fn transform_errors(options: &TransformOptions) -> Vec<Message> {
    let result = api::transform("", options.clone());
    assert!(result.warnings.is_empty());
    assert!(result.code.is_empty());
    assert!(result.map.is_empty());
    check_fields(&result.errors);
    compare_go("transform", options, &result.errors);
    result.errors
}

#[test]
fn collects_define_before_keep_names() {
    let options = TransformOptions {
        define: define(),
        ..options()
    };
    let errors = transform_errors(&options);
    assert_eq!(
        errors.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
        [DEFINE, &keep(" (\"chrome36\")")]
    );
}

#[test]
fn keeps_all_linked_flag_errors_with_default_and_explicit_sourcefile() {
    for sourcefile in ["", "input.js"] {
        for (map, legal) in [(true, false), (false, true), (true, true)] {
            let options = TransformOptions {
                sourcefile: sourcefile.into(),
                sourcemap: if map {
                    BuildSourceMap::Linked
                } else {
                    BuildSourceMap::None
                },
                legal_comments: if legal {
                    BuildLegalComments::Linked
                } else {
                    BuildLegalComments::None
                },
                ..options()
            };
            let errors = transform_errors(&options);
            let mut expected = vec![keep(" (\"chrome36\")")];
            if map {
                expected.push(MAP.into());
            }
            if legal {
                expected.push(LEGAL.into());
            }
            assert_eq!(
                errors.iter().map(|m| &m.text).collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn defines_precede_linked_flags_when_keep_names_is_disabled_or_supported() {
    for supported in [false, true] {
        let options = TransformOptions {
            keep_names: supported,
            supported: HashMap::from([("function-name-configurable".into(), true)]),
            define: define(),
            sourcemap: BuildSourceMap::Linked,
            legal_comments: BuildLegalComments::Linked,
            ..options()
        };
        let errors = transform_errors(&options);
        assert_eq!(
            errors.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            [DEFINE, MAP, LEGAL]
        );
    }
}

#[test]
fn target_supported_and_define_errors_precede_keep_names_and_linked_flags() {
    let options = TransformOptions {
        engines: vec![Engine {
            name: EngineName::Chrome,
            version: "invalid".into(),
        }],
        supported: HashMap::from([
            ("UNKNOWN".into(), false),
            ("function-name-configurable".into(), false),
        ]),
        define: define(),
        sourcemap: BuildSourceMap::Linked,
        legal_comments: BuildLegalComments::Linked,
        ..options()
    };
    let errors = transform_errors(&options);
    assert_eq!(errors.len(), 6);
    assert_eq!(errors[0].text, "Invalid version: \"invalid\"");
    assert_eq!(
        errors[1].text,
        "\"UNKNOWN\" is not a valid feature name for the \"supported\" setting"
    );
    assert_eq!(errors[2].text, DEFINE);
    assert_eq!(errors[3].text, keep(""));
    assert_eq!(errors[4].text, MAP);
    assert_eq!(errors[5].text, LEGAL);
}

#[test]
fn build_and_context_keep_their_combined_option_order() {
    for supported in [false, true] {
        let options = TransformOptions {
            define: define(),
            supported: HashMap::from([("function-name-configurable".into(), supported)]),
            sourcemap: BuildSourceMap::Linked,
            legal_comments: BuildLegalComments::Linked,
            ..options()
        };
        let build_options = BuildOptions {
            keep_names: options.keep_names,
            engines: options.engines.clone(),
            supported: options.supported.clone(),
            define: options.define.clone(),
            sourcemap: options.sourcemap,
            legal_comments: options.legal_comments,
            outfile: "out.js".into(),
            tsconfig_raw: "{}".into(),
            stdin: Some(BuildStdin {
                sourcefile: "input.js".into(),
                ..BuildStdin::default()
            }),
            ..BuildOptions::default()
        };
        let built = api::build(build_options.clone());
        assert!(built.warnings.is_empty());
        assert!(built.output_files.is_empty());
        let context_errors = match api::context(build_options) {
            Err(error) => error.errors,
            Ok(context) => {
                context.dispose();
                panic!("invalid define unexpectedly accepted")
            }
        };
        for (mode, errors) in [("build", built.errors), ("context", context_errors)] {
            check_fields(&errors);
            let mut expected = vec![DEFINE.to_string()];
            if !supported {
                expected.push(keep(" (\"chrome36\" + 1 override)"));
            }
            assert_eq!(
                errors.iter().map(|m| &m.text).collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
            compare_go(mode, &options, &errors);
        }
    }
}

#[test]
fn collects_jsx_and_regex_errors_before_keep_names_and_linked_flags() {
    for loader in [
        Loader::Js,
        Loader::Css,
        Loader::Json,
        Loader::Text,
        Loader::Empty,
    ] {
        let options = TransformOptions {
            loader,
            define: define(),
            jsx_factory: "bad..factory".into(),
            jsx_fragment: "bad..fragment".into(),
            mangle_props: "(?=x)".into(),
            sourcemap: BuildSourceMap::Linked,
            legal_comments: BuildLegalComments::Linked,
            ..options()
        };
        let errors = transform_errors(&options);
        assert_eq!(errors.len(), 7);
        assert_eq!(errors[0].text, DEFINE);
        // Order assertions survive the separately owned diagnostic text fixes.
        assert!(errors[1].text.ends_with("factory: \"bad..factory\""));
        assert!(errors[2].text.ends_with("fragment: \"bad..fragment\""));
        assert!(errors[3].text.starts_with("The \"mangle props\" setting"));
        assert_eq!(errors[4].text, keep(" (\"chrome36\")"));
        assert_eq!(errors[5].text, MAP);
        assert_eq!(errors[6].text, LEGAL);
    }
}
