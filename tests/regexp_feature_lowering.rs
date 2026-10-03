use std::{collections::HashMap, process::Command, sync::Arc};

use esbuild_rs::{
    api::{
        self, BuildSourceMap, Engine, EngineName, Location, LogLevel, Message, Target,
        TransformOptions,
    },
    internal::{
        compat::JsFeature,
        js_parser,
        logger::{DeferLogKind, Log, MsgId, MsgKind, PrettyPaths, Source},
    },
};
use serde_json::{Value, json};

fn cases() -> Vec<Value> {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/regexp_feature_lowering_go.json")).unwrap();
    assert_eq!(
        fixture["upstream_revision"],
        "6ff1d8b0d8c134e867a397eef39702a223ebef9e"
    );
    let cases = fixture["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 96);
    cases
}

fn apply_target(options: &mut TransformOptions, target: &str) {
    options.target = match target {
        "esnext" => Target::EsNext,
        "es5" => Target::Es5,
        "es6" | "es2015" => Target::Es2015,
        "es2017" => Target::Es2017,
        "es2018" => Target::Es2018,
        "es2021" => Target::Es2021,
        "es2022" => Target::Es2022,
        "es2024" => Target::Es2024,
        _ => {
            let (name, version) = if let Some(version) = target.strip_prefix("node") {
                (EngineName::Node, version)
            } else {
                (EngineName::Chrome, target.strip_prefix("chrome").unwrap())
            };
            options.engines.push(Engine {
                name,
                version: version.into(),
            });
            return;
        }
    };
}

fn options(value: &Value) -> TransformOptions {
    let mut options = TransformOptions {
        sourcefile: value["sourcefile"].as_str().unwrap().into(),
        ..TransformOptions::default()
    };
    if let Some(target) = value["target"].as_str() {
        apply_target(&mut options, target);
    } else if let Some(targets) = value["target"].as_array() {
        for target in targets {
            apply_target(&mut options, target.as_str().unwrap());
        }
    }
    if let Some(supported) = value["supported"].as_object() {
        options.supported = supported
            .iter()
            .map(|(key, value)| (key.clone(), value.as_bool().unwrap()))
            .collect();
    }
    if let Some(overrides) = value["logOverride"].as_object() {
        options.log_override = overrides
            .iter()
            .map(|(key, value)| {
                let level = match value.as_str().unwrap() {
                    "warning" => LogLevel::Warning,
                    "error" => LogLevel::Error,
                    "silent" => LogLevel::Silent,
                    other => panic!("unhandled fixture log level {other}"),
                };
                (key.clone(), level)
            })
            .collect();
    }
    let minify = value["minify"].as_bool().unwrap_or(false);
    options.minify_syntax = minify || value["minifySyntax"].as_bool().unwrap_or(false);
    options.minify_whitespace = minify;
    options.minify_identifiers = minify;
    options.ascii_only = value["charset"] != "utf8";
    if value["sourcemap"] == "external" {
        options.sourcemap = BuildSourceMap::External;
    }
    options
}

fn location(location: &Location) -> Value {
    json!({
        "file": location.file, "namespace": location.namespace,
        "line": location.line, "column": location.column, "length": location.length,
        "lineText": location.line_text, "suggestion": location.suggestion,
    })
}

fn message(message: &Message) -> Value {
    assert!(message.detail.is_none());
    json!({
        "id": message.id, "pluginName": message.plugin_name, "text": message.text,
        "location": message.location.as_ref().map(location),
        "notes": message.notes.iter().map(|note| {
            json!({"text":note.text,"location":note.location.as_ref().map(location)})
        }).collect::<Vec<_>>()
    })
}

fn check(case: &Value) -> String {
    let result = api::transform(case["input"].as_str().unwrap(), options(&case["options"]));
    let code = String::from_utf8(result.code).unwrap();
    let mut actual = json!({
        "errors":result.errors.iter().map(message).collect::<Vec<_>>(),
        "warnings":result.warnings.iter().map(message).collect::<Vec<_>>()
    });
    if result.errors.is_empty() {
        actual["code"] = json!(code);
        actual["map"] = json!(String::from_utf8(result.map).unwrap());
    }
    let mut expected = case["response"].clone();
    expected.as_object_mut().unwrap().remove("runtime");
    assert_eq!(actual, expected, "{}", case["name"]);
    code
}

#[test]
fn native_api_matches_all_independent_go_regexp_cases_exactly() {
    // Exact bytes, maps, diagnostic IDs/ranges/notes, target boundaries,
    // supported overrides, scan exclusions, validation precedence, and shadowing.
    for case in cases() {
        check(&case);
    }
}

#[test]
fn fallback_constructor_effects_and_global_binding_survive_minification() {
    for case in cases().iter().filter(|case| case["runtime"] == true) {
        let code = check(case);
        let encoded_code = serde_json::to_string(&code).unwrap();
        let script = format!(
            r"const vm=require('node:vm');const calls=[];const context={{}};
context.RegExp=function(pattern,flags){{calls.push([pattern,flags]);return new RegExp(pattern,flags)}};
vm.runInNewContext({encoded_code},context);
console.log(JSON.stringify({{calls,result:{{source:context.result.source,flags:context.result.flags}}}}));",
        );
        let output = Command::new("node").arg("-e").arg(script).output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(actual, case["response"]["runtime"], "{}", case["name"]);
    }
}

#[test]
fn unsupported_regexp_is_debug_by_default_and_pattern_detection_precedes_flags() {
    let log = Log::new_defer(DeferLogKind::All, HashMap::new());
    let source = "result = /(?<=x)/s";
    let (_, ok) = js_parser::parse(
        log.clone(),
        Source {
            pretty_paths: PrettyPaths {
                abs: "regexp.js".into(),
                rel: "regexp.js".into(),
            },
            contents: Arc::from(source.as_bytes()),
            ..Source::default()
        },
        js_parser::Options {
            unsupported_js_features: JsFeature::REGEXP_LOOKBEHIND_ASSERTIONS
                | JsFeature::REGEXP_DOT_ALL_FLAG,
            original_target_env: "\"es2017\"".into(),
            ..js_parser::Options::default()
        },
    );
    assert!(ok);
    let messages = log.done();
    assert_eq!(messages.len(), 1);
    let diagnostic = &messages[0];
    assert_eq!(diagnostic.kind, MsgKind::Debug);
    assert_eq!(diagnostic.id, MsgId::JsUnsupportedRegExp);
    assert_eq!(
        diagnostic.data.text,
        "Lookbehind assertions in regular expressions are not available in the configured target environment (\"es2017\")"
    );
    let location = diagnostic.data.location.as_ref().unwrap();
    assert_eq!(location.column, 11);
    assert_eq!(location.length, 3);
    assert_eq!(diagnostic.notes.len(), 1);
    assert!(diagnostic.notes[0].location.is_none());
}
