//! Exact comparisons against the original JS/TS parser helper expectations.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::internal::{
    ast::SymbolMap,
    compat::JsFeature,
    config::{JsxOptions, MaybeBool, TsOptions},
    js_printer,
    logger::{DeferLogKind, Log, MsgKind, OutputOptions, Path, PrettyPaths, Source, TerminalInfo},
    renamer::new_no_op_renamer,
};

fn maybe_bool(value: &Value) -> MaybeBool {
    match value.as_u64().unwrap_or_default() {
        0 => MaybeBool::Unspecified,
        1 => MaybeBool::True,
        2 => MaybeBool::False,
        _ => panic!("invalid upstream MaybeBool"),
    }
}

fn run_case(case: &Value) -> Result<(), String> {
    let options = &case["options"];
    for key in options.as_object().expect("options object").keys() {
        assert!(
            matches!(
                key.as_str(),
                "UnsupportedJSFeatures"
                    | "MinifySyntax"
                    | "JSX"
                    | "OmitJSXRuntimeForTests"
                    | "ASCIIOnly"
                    | "TS"
            ),
            "unmapped upstream option {key}"
        );
    }
    let input = STANDARD
        .decode(case["source_base64"].as_str().unwrap())
        .unwrap();
    let expected = STANDARD
        .decode(case["expected_base64"].as_str().unwrap())
        .unwrap();
    let diagnostic = case["kind"] == "diagnostic";
    assert!(
        diagnostic || case["kind"] == "print",
        "unknown parser fixture kind"
    );
    let unsupported_features = JsFeature::from_bits(
        options["UnsupportedJSFeatures"]
            .as_str()
            .map_or(0, |bits| bits.parse().unwrap()),
    );
    let ascii_only = options["ASCIIOnly"].as_bool().unwrap_or_default();
    let mut parser_options = super::Options {
        unsupported_js_features: unsupported_features,
        minify_syntax: options["MinifySyntax"].as_bool().unwrap_or_default(),
        ascii_only,
        omit_runtime_for_tests: !diagnostic,
        omit_jsx_runtime_for_tests: options["OmitJSXRuntimeForTests"]
            .as_bool()
            .unwrap_or_default(),
        ..super::Options::default()
    };
    if let Some(jsx) = options.get("JSX") {
        // The captured parser helpers only use the default factory and fragment.
        for name in ["Factory", "Fragment"] {
            assert!(
                jsx[name]["Constant"].is_null() && jsx[name]["Parts"].is_null(),
                "unmapped JSX {name}"
            );
        }
        parser_options.jsx = JsxOptions {
            parse: jsx["Parse"].as_bool().unwrap(),
            preserve: jsx["Preserve"].as_bool().unwrap(),
            automatic_runtime: jsx["AutomaticRuntime"].as_bool().unwrap(),
            import_source: jsx["ImportSource"].as_str().unwrap().into(),
            development: jsx["Development"].as_bool().unwrap(),
            side_effects: jsx["SideEffects"].as_bool().unwrap(),
            ..JsxOptions::default()
        };
    }
    if let Some(ts) = options.get("TS") {
        parser_options.ts = TsOptions {
            parse: ts["Parse"].as_bool().unwrap(),
            no_ambiguous_less_than: ts["NoAmbiguousLessThan"].as_bool().unwrap(),
            ..TsOptions::default()
        };
        for (key, value) in ts["Config"].as_object().unwrap() {
            match key.as_str() {
                "UseDefineForClassFields" => {
                    parser_options.ts.config.use_define_for_class_fields = maybe_bool(value)
                }
                "ExperimentalDecorators" => {
                    parser_options.ts.config.experimental_decorators = maybe_bool(value)
                }
                _ => assert_eq!(value.as_u64(), Some(0), "unmapped TS option {key}"),
            }
        }
    }
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let source = Source {
        key_path: Path {
            text: "<stdin>".into(),
            ..Path::default()
        },
        pretty_paths: PrettyPaths {
            abs: "<stdin>".into(),
            rel: "<stdin>".into(),
        },
        identifier_name: "stdin".into(),
        contents: Arc::from(input),
        ..Source::default()
    };
    let (ast, ok) = super::parse(log.clone(), source, parser_options);
    let messages = log.done();
    let diagnostics: Vec<u8> = messages
        .iter()
        .filter(|msg| diagnostic || msg.kind != MsgKind::Warning)
        .flat_map(|msg| msg.to_bytes(&OutputOptions::default(), TerminalInfo::default()))
        .collect();
    let actual = if diagnostic {
        diagnostics
    } else {
        if !ok || !diagnostics.is_empty() {
            return Err(format!(
                "parse failed: {}",
                String::from_utf8_lossy(&diagnostics)
            ));
        }
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        js_printer::print(
            &ast,
            &renamer,
            js_printer::Options {
                unsupported_features,
                ascii_only,
                ..js_printer::Options::default()
            },
        )
        .js
    };
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "expected {:?}\nactual {:?}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&actual)
        ))
    }
}

fn run_corpus(audit: bool) {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../../../tests/upstream/js_parser.json")).unwrap();
    assert_eq!(
        cases.len(),
        8_643,
        "upstream JS/TS parser case count changed"
    );
    let active: Vec<usize> = serde_json::from_str(include_str!(
        "../../../tests/upstream/js_parser_active.json"
    ))
    .unwrap();
    assert_eq!(
        active.len(),
        6_861,
        "active JS/TS parser case count changed"
    );
    if !audit {
        assert!(!active.is_empty(), "no upstream parser coverage is active");
    }
    let active_set: HashSet<_> = active.iter().copied().collect();
    assert_eq!(
        active.len(),
        active_set.len(),
        "duplicate active parser fixture"
    );
    assert!(
        active.iter().all(|index| *index < cases.len()),
        "invalid active parser fixture"
    );
    let filter = std::env::var("ESBUILD_RS_UPSTREAM_PARSER_TEST").ok();
    let selected_index = std::env::var("ESBUILD_RS_UPSTREAM_PARSER_INDEX")
        .ok()
        .map(|index| index.parse::<usize>().expect("parser fixture index"));
    let mut report = Vec::new();
    let mut failures = Vec::new();
    let mut passed = 0;
    for (index, case) in cases.iter().enumerate() {
        if selected_index.is_some_and(|selected| selected != index)
            || filter
                .as_ref()
                .is_some_and(|name| case["upstream_test"] != *name)
            || (!audit
                && filter.is_none()
                && selected_index.is_none()
                && !active_set.contains(&index))
        {
            continue;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(case)))
            .unwrap_or_else(|_| Err("parser/printer panicked".into()));
        let error = result.err();
        if let Some(error) = &error {
            failures.push(format!(
                "#{index} {}:{} {}: {error}",
                case["file"], case["line"], case["upstream_test"]
            ));
        } else {
            passed += 1;
        }
        report.push(
            json!({"index": index, "file": case["file"], "line": case["line"],
            "upstream_test": case["upstream_test"], "passed": error.is_none(), "error": error}),
        );
    }
    assert!(!report.is_empty(), "no parser fixtures matched the filter");
    println!(
        "Upstream JS/TS parser cases: {passed} passed, {} failed, {} checked",
        failures.len(),
        report.len()
    );
    if let Ok(path) = std::env::var("ESBUILD_RS_UPSTREAM_PARSER_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write parser audit report");
    }
    if !audit {
        assert!(
            failures.is_empty(),
            "{}",
            failures
                .into_iter()
                .take(30)
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }
}

#[test]
fn matches_pinned_upstream_active_js_parser_corpus() {
    run_corpus(false);
}

#[test]
#[ignore = "audits the entire JS/TS parser backlog; failures are reported separately"]
fn audits_pinned_upstream_js_parser_corpus() {
    run_corpus(true);
}
