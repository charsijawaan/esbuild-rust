//! Transform input-map composition and sourcefile isolation regressions.

use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::{
    api::{
        BuildFormat, BuildSourceMap, BuildSourcesContent, Loader, LogLevel, Message,
        TransformOptions, TransformResult, transform,
    },
    internal::{
        js_parser,
        logger::{DeferLogKind, Log, Source},
    },
};
use serde_json::{Value, json};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "esbuild-team-source-maps-{}-{stamp}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn input_with_comment(input: &str, comment: &str, css: bool) -> String {
    if css {
        format!("{input}\n/*# sourceMappingURL={comment} */")
    } else {
        format!("{input}\n//# sourceMappingURL={comment}")
    }
}

fn data_url(map: &str, base64: bool) -> String {
    if base64 {
        format!("data:application/json;base64,{}", STANDARD.encode(map))
    } else {
        // A percent-escaped URL without base64 exercises the other decoder.
        let escaped: String = url::form_urlencoded::byte_serialize(map.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        format!("data:application/json;charset=utf-8,{escaped}")
    }
}

fn source_map(result: &TransformResult) -> Value {
    if result.map.is_empty() {
        let code = String::from_utf8_lossy(&result.code);
        let encoded = code
            .split("sourceMappingURL=data:application/json;base64,")
            .nth(1)
            .expect("inline output source map")
            .split_whitespace()
            .next()
            .unwrap();
        serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap()
    } else {
        serde_json::from_slice(&result.map).unwrap()
    }
}

fn location_json(location: Option<&esbuild_rs::api::Location>) -> Value {
    location.map_or(Value::Null, |location| {
        json!({
            "File": location.file, "Namespace": location.namespace,
            "Line": location.line, "Column": location.column, "Length": location.length,
            "LineText": location.line_text, "Suggestion": location.suggestion,
        })
    })
}

fn messages_json(messages: &[Message]) -> Value {
    if messages.is_empty() {
        return Value::Null;
    }
    Value::Array(
        messages
            .iter()
            .map(|message| {
                json!({
                    "ID": message.id, "PluginName": message.plugin_name, "Text": message.text,
                    "Location": location_json(message.location.as_ref()),
                    "Notes": if message.notes.is_empty() { Value::Null } else {
                        Value::Array(message.notes.iter().map(|note| json!({
                            "Text": note.text, "Location": location_json(note.location.as_ref()),
                        })).collect())
                    },
                    "Detail": null,
                })
            })
            .collect(),
    )
}

fn go_request(input: &str, options: &TransformOptions) -> Value {
    let loader = match options.loader {
        Loader::None => 0,
        Loader::Css => 4,
        Loader::GlobalCss => 9,
        Loader::Js => 10,
        Loader::Jsx => 12,
        Loader::LocalCss => 13,
        Loader::Ts => 15,
        Loader::Tsx => 16,
        _ => panic!("unmapped test loader"),
    };
    let mode = match options.sourcemap {
        BuildSourceMap::None => 0,
        BuildSourceMap::Inline => 1,
        BuildSourceMap::External => 3,
        BuildSourceMap::InlineAndExternal => 4,
        BuildSourceMap::Linked => panic!("linked transform map"),
    };
    let overrides: HashMap<_, _> = options
        .log_override
        .iter()
        .map(|(name, level)| {
            let level = match level {
                LogLevel::Silent => 0,
                LogLevel::Warning => 4,
                LogLevel::Error => 5,
                _ => panic!("unmapped test log level"),
            };
            (name, level)
        })
        .collect();
    json!({"Input": input, "Options": {
        "Loader": loader, "Sourcefile": options.sourcefile, "Sourcemap": mode,
        "Format": match options.format {
            BuildFormat::Default => 0, BuildFormat::CommonJs => 2, BuildFormat::EsModule => 3,
            BuildFormat::Iife => panic!("unmapped test format"),
        },
        "SourceRoot": options.source_root,
        "SourcesContent": usize::from(options.sources_content == BuildSourcesContent::Exclude),
        "MinifyWhitespace": options.minify_whitespace,
        "MinifyIdentifiers": options.minify_identifiers,
        "MinifySyntax": options.minify_syntax,
        "Charset": if options.ascii_only { 1 } else { 2 },
        "Banner": options.banner, "Footer": options.footer, "LogOverride": overrides,
    }})
}

fn without_inline_map(code: &str) -> &str {
    code.split("//# sourceMappingURL=data:")
        .next()
        .unwrap()
        .split("/*# sourceMappingURL=data:")
        .next()
        .unwrap()
}

fn compare_map_positions(rust: &Value, go: &Value, code: &str) -> Value {
    // Node independently decodes both VLQ streams. Compare every mapping
    // position that Rust emits. The Go printer currently emits extra positions;
    // do not equate these checks with exact mapping-string parity.
    const SCRIPT: &str = r"
const {SourceMap} = require('node:module');
const data = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
const rust = new SourceMap(data.rust), go = new SourceMap(data.go);
let samples = 0, mismatches = [];
data.code.split(/\r\n|[\r\n\u2028\u2029]/).forEach((text, line) => {
  for (let column = 0; column <= text.length; column++) {
    const entry = rust.findEntry(line, column);
    if (entry.generatedLine !== line || entry.generatedColumn !== column) continue;
    const reference = go.findEntry(line, column);
    for (const key of ['originalSource', 'originalLine', 'originalColumn', 'name']) {
      if (entry[key] !== reference[key]) mismatches.push({line, column, key, rust: entry[key], go: reference[key]});
    }
    samples++;
  }
});
console.log(JSON.stringify({samples, mismatches, rawMappingsEqual: data.rust.mappings === data.go.mappings}));
";
    let mut child = Command::new("node")
        .args(["-e", SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&json!({
                "rust": rust, "go": go, "code": code,
            }))
            .unwrap(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// Optional independent reference: this executable calls the pinned Go API,
// not the CLI or the JavaScript API test runner. Without it, fixed assertions
// below still run and no Go comparison is claimed. Chained positions must
// match; fallback printer differences are recorded without claiming parity.
fn compare_go(input: &str, options: &TransformOptions, result: &TransformResult) -> Option<Value> {
    let executable = std::env::var_os("ESBUILD_RS_TEAM_GO_TRANSFORM")?;
    let request = go_request(input, options);
    let mut child = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("pinned Go transform harness");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let go: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(messages_json(&result.errors), go["Errors"], "{request}");
    assert_eq!(messages_json(&result.warnings), go["Warnings"], "{request}");
    let code = String::from_utf8_lossy(&result.code);
    let go_code = go["Code"].as_str().unwrap();
    assert_eq!(
        without_inline_map(&code),
        without_inline_map(go_code),
        "{request}"
    );
    assert_eq!(
        result.map.is_empty(),
        go["Map"].as_str().unwrap().is_empty()
    );
    let mut report = json!({"options": request["Options"], "codeAndDiagnosticsEqual": true});
    if options.sourcemap == BuildSourceMap::None || !result.errors.is_empty() {
        assert_eq!(go["Map"], "");
    } else {
        let mut rust_map = source_map(result);
        let mut go_map = source_map(&TransformResult {
            code: go_code.as_bytes().to_vec(),
            map: go["Map"].as_str().unwrap().as_bytes().to_vec(),
            ..TransformResult::default()
        });
        report["mappingChecks"] =
            compare_map_positions(&rust_map, &go_map, without_inline_map(&code));
        let chained = rust_map["sources"] != json!([options.sourcefile]);
        report["inputMapChained"] = json!(chained);
        assert!(report["mappingChecks"]["samples"].as_u64().unwrap() > 0);
        if chained {
            assert_eq!(
                report["mappingChecks"]["mismatches"],
                json!([]),
                "{request}"
            );
        }
        rust_map.as_object_mut().unwrap().remove("mappings");
        go_map.as_object_mut().unwrap().remove("mappings");
        assert_eq!(rust_map, go_map, "{request}");
        report["mapMetadataEqual"] = json!(true);
    }
    if let Some(path) = std::env::var_os("ESBUILD_RS_TEAM_SOURCE_MAP_REPORT") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let mut line = serde_json::to_vec(&report).unwrap();
        line.push(b'\n');
        file.write_all(&line).unwrap();
    }
    Some(go)
}

#[test]
fn inline_transform_maps_chain_sources_contents_roots_names_and_positions() {
    let map = json!({
        "version": 3, "sourceRoot": "https://example.com/src/",
        "sources": ["first.scss", "second.scss"],
        "sourcesContent": ["", "original source π"],
        "names": ["originalToken"], "mappings": "ACwCGA",
    })
    .to_string();
    for loader in [Loader::Js, Loader::Ts, Loader::Css, Loader::LocalCss] {
        let css = matches!(loader, Loader::Css | Loader::LocalCss);
        let source = if css {
            ".button { color: red }"
        } else {
            "throw new Error('mapped');"
        };
        for base64 in [false, true] {
            let input = input_with_comment(source, &data_url(&map, base64), css);
            for minify in [false, true] {
                for exclude in [false, true] {
                    for mode in [
                        BuildSourceMap::External,
                        BuildSourceMap::Inline,
                        BuildSourceMap::InlineAndExternal,
                    ] {
                        let options = TransformOptions {
                            sourcefile: if css { "input.css" } else { "input.js" }.into(),
                            loader,
                            sourcemap: mode,
                            minify_whitespace: minify,
                            minify_identifiers: minify,
                            minify_syntax: minify,
                            sources_content: if exclude {
                                BuildSourcesContent::Exclude
                            } else {
                                BuildSourcesContent::Include
                            },
                            source_root: "https://output.example/".into(),
                            ..TransformOptions::default()
                        };
                        let result = transform(&input, options.clone());
                        assert!(result.errors.is_empty(), "{:?}", result.errors);
                        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
                        let output = source_map(&result);
                        assert_eq!(
                            output["sources"],
                            json!([
                                "https://example.com/src/first.scss",
                                "https://example.com/src/second.scss"
                            ])
                        );
                        assert_eq!(output["sourceRoot"], "https://output.example/");
                        assert_eq!(output["names"], json!(["originalToken"]));
                        if exclude {
                            assert!(output.get("sourcesContent").is_none());
                        } else {
                            assert_eq!(output["sourcesContent"], json!(["", "original source π"]));
                        }
                        let parsed = js_parser::parse_source_map(
                            Log::new_defer(DeferLogKind::All, HashMap::new()),
                            Source {
                                contents: output.to_string().into_bytes().into(),
                                ..Source::default()
                            },
                        )
                        .unwrap();
                        assert!(!parsed.mappings.is_empty());
                        assert!(parsed.mappings.iter().all(|mapping| {
                            (
                                mapping.source_index,
                                mapping.original_line,
                                mapping.original_column,
                            ) == (1, 40, 3)
                        }));
                        assert_eq!(result.map.is_empty(), mode == BuildSourceMap::Inline);
                        compare_go(&input, &options, &result);
                    }
                }
            }
        }
    }
}

#[test]
fn transforms_do_not_read_external_maps_or_missing_original_contents() {
    let fixture = Fixture::new();
    let original = fixture.0.join("original.ts");
    std::fs::write(&original, "HOST CONTENT MUST NOT APPEAR").unwrap();
    let original_url = url::Url::from_file_path(&original).unwrap().to_string();
    let map_path = fixture.0.join("input.map");
    let map = json!({"version": 3, "sources": [original_url], "names": [], "mappings": "AAwCA"});
    std::fs::write(&map_path, map.to_string()).unwrap();
    for (loader, source, css) in [
        (Loader::Js, "console.log(1);", false),
        (Loader::Css, "a { color: red }", true),
    ] {
        for comment in [
            "input.map".into(),
            url::Url::from_file_path(&map_path).unwrap().to_string(),
        ] {
            for level in [None, Some(LogLevel::Warning), Some(LogLevel::Error)] {
                let mut options = TransformOptions {
                    sourcefile: fixture
                        .0
                        .join(if css { "input.css" } else { "input.js" })
                        .to_string_lossy()
                        .into_owned(),
                    loader,
                    sourcemap: BuildSourceMap::External,
                    ..TransformOptions::default()
                };
                if let Some(level) = level {
                    options.log_override.insert(
                        if comment.starts_with("file:") {
                            "missing-source-map"
                        } else {
                            "unsupported-source-map-comment"
                        }
                        .into(),
                        level,
                    );
                }
                let input = input_with_comment(source, &comment, css);
                let result = transform(&input, options.clone());
                assert_eq!(
                    result.errors.len(),
                    usize::from(level == Some(LogLevel::Error))
                );
                assert_eq!(
                    result.warnings.len(),
                    usize::from(level == Some(LogLevel::Warning))
                );
                if result.errors.is_empty() {
                    assert_eq!(source_map(&result)["sources"], json!([options.sourcefile]));
                } else {
                    assert!(result.code.is_empty() && result.map.is_empty());
                }
                compare_go(&input, &options, &result);
            }
        }
        for content in [
            None,
            Some(Value::Null),
            Some(json!("")),
            Some(json!("embedded π")),
        ] {
            let mut map = map.clone();
            if let Some(content) = &content {
                map["sourcesContent"] = json!([content]);
            }
            let input = input_with_comment(source, &data_url(&map.to_string(), true), css);
            let options = TransformOptions {
                sourcefile: "input".into(),
                loader,
                sourcemap: BuildSourceMap::External,
                ..TransformOptions::default()
            };
            let result = transform(&input, options.clone());
            assert!(result.errors.is_empty() && result.warnings.is_empty());
            let output = source_map(&result);
            assert_eq!(output["sources"], json!([original_url]));
            assert_eq!(
                output["sourcesContent"],
                json!([content.unwrap_or(Value::Null)])
            );
            compare_go(&input, &options, &result);
        }
    }
}

#[test]
fn malformed_transform_maps_keep_locations_notes_severity_and_fallback() {
    for (loader, source, css) in [
        (Loader::Js, "console.log(1);", false),
        (Loader::Css, "a { color: red }", true),
    ] {
        for (map, warning) in [
            ("{", false),
            ("[]", false),
            (r#"{"version":3,"sections":false}"#, false),
            (
                r#"{"version":3,"sources":["original"],"mappings":"ACAA"}"#,
                true,
            ),
            (
                r#"{"version":3,"sources":["original"],"names":[],"mappings":"AAAAC"}"#,
                true,
            ),
            (
                r#"{"version":3,"sources":["original"],"mappings":"!"}"#,
                true,
            ),
        ] {
            let input = input_with_comment(source, &data_url(map, true), css);
            let options = TransformOptions {
                sourcefile: "input".into(),
                loader,
                sourcemap: BuildSourceMap::External,
                ..TransformOptions::default()
            };
            let result = transform(&input, options.clone());
            let messages = if warning {
                &result.warnings
            } else {
                &result.errors
            };
            assert_eq!(messages.len(), 1, "{map}: {result:?}");
            let message = &messages[0];
            assert_eq!(
                message.location.as_ref().unwrap().file,
                "input#sourceMappingURL"
            );
            assert_eq!(message.notes.len(), 1);
            assert_eq!(
                message.notes[0].text,
                "This source map came from the file \"input\" here:"
            );
            let note = message.notes[0].location.as_ref().unwrap();
            assert_eq!((note.line, note.column), (2, 21));
            if warning {
                assert_eq!(message.id, "invalid-source-mappings");
                assert_eq!(source_map(&result)["sources"], json!(["input"]));
            } else {
                assert!(result.code.is_empty() && result.map.is_empty());
            }
            compare_go(&input, &options, &result);
            if warning {
                for level in [LogLevel::Silent, LogLevel::Error] {
                    let mut options = options.clone();
                    options
                        .log_override
                        .insert("invalid-source-mappings".into(), level);
                    let result = transform(&input, options.clone());
                    assert!(result.warnings.is_empty());
                    assert_eq!(result.errors.len(), usize::from(level == LogLevel::Error));
                    compare_go(&input, &options, &result);
                }
            }
            let mut disabled = options;
            disabled.sourcemap = BuildSourceMap::None;
            let result = transform(&input, disabled.clone());
            assert!(result.errors.is_empty() && result.warnings.is_empty());
            assert!(result.map.is_empty());
            compare_go(&input, &disabled, &result);
        }
    }
}

#[test]
fn malformed_data_url_decoding_warns_and_honors_overrides_without_panicking() {
    for (loader, source, css) in [
        (Loader::Js, "console.log(1);", false),
        (Loader::Css, "a { color: red }", true),
    ] {
        for comment in [
            "data:application/json;base64,!",
            "data:application/json,%XX",
        ] {
            let input = input_with_comment(source, comment, css);
            for level in [None, Some(LogLevel::Silent), Some(LogLevel::Error)] {
                let mut options = TransformOptions {
                    sourcefile: "input".into(),
                    loader,
                    sourcemap: BuildSourceMap::External,
                    ..TransformOptions::default()
                };
                if let Some(level) = level {
                    options
                        .log_override
                        .insert("unsupported-source-map-comment".into(), level);
                }
                let result = transform(&input, options);
                assert_eq!(
                    result.errors.len(),
                    usize::from(level == Some(LogLevel::Error))
                );
                assert_eq!(result.warnings.len(), usize::from(level.is_none()));
                if let Some(message) = result.errors.first().or_else(|| result.warnings.first()) {
                    assert_eq!(message.id, "unsupported-source-map-comment");
                    assert!(
                        message
                            .text
                            .starts_with("Unsupported source map comment: could not decode")
                    );
                    let location = message.location.as_ref().unwrap();
                    assert_eq!(
                        (
                            location.file.as_str(),
                            location.line,
                            location.column,
                            location.length
                        ),
                        ("input", 2, 21, comment.len())
                    );
                    assert!(message.notes.is_empty());
                }
                if result.errors.is_empty() {
                    assert_eq!(source_map(&result)["sources"], json!(["input"]));
                } else {
                    assert!(result.code.is_empty() && result.map.is_empty());
                }
            }
        }
    }
}

#[test]
fn indexed_javascript_input_maps_preserve_unmapped_lines_and_multiple_sources() {
    let map = json!({"version": 3, "sections": [
        {"offset": {"line": 0, "column": 0}, "map": {
            "version": 3, "sources": ["first.ts"], "sourcesContent": ["first"],
            "names": [], "mappings": "AAUA",
        }},
        {"offset": {"line": 2, "column": 0}, "map": {
            "version": 3, "sourceRoot": "../original/", "sources": ["last.ts"],
            "sourcesContent": ["last"], "names": [], "mappings": "AAoBA",
        }},
    ]})
    .to_string();
    for loader in [Loader::Js, Loader::Ts] {
        let input = input_with_comment(
            "first();\nunmapped();\nlast();",
            &data_url(&map, true),
            false,
        );
        let options = TransformOptions {
            sourcefile: "generated/input".into(),
            loader,
            sourcemap: BuildSourceMap::External,
            ..TransformOptions::default()
        };
        let result = transform(&input, options.clone());
        assert!(result.errors.is_empty() && result.warnings.is_empty());
        let output = source_map(&result);
        assert_eq!(
            output["sources"],
            json!(["first.ts", "../original/last.ts"])
        );
        assert_eq!(output["sourcesContent"], json!(["first", "last"]));
        let parsed = js_parser::parse_source_map(
            Log::new_defer(DeferLogKind::All, HashMap::new()),
            Source {
                contents: output.to_string().into_bytes().into(),
                ..Source::default()
            },
        )
        .unwrap();
        assert!(parsed.mappings.iter().all(|mapping| {
            (mapping.source_index, mapping.original_line) == (0, 10)
                || (mapping.source_index, mapping.original_line) == (1, 20)
        }));
        let code = String::from_utf8_lossy(&result.code);
        let line = code[..code.find("unmapped").unwrap()]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        assert!(
            parsed.find(i32::try_from(line).unwrap(), 0).is_none(),
            "{loader:?}: {code}\n{output}"
        );
        compare_go(&input, &options, &result);
    }
}

fn node_stack(code: &[u8], path: &std::path::Path) {
    std::fs::write(path, code).unwrap();
    let output = Command::new("node")
        .arg("--enable-source-maps")
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Error: mapped"), "{stderr}");
    assert!(stderr.contains("original.ts:41:4"), "{stderr}");
}

#[test]
fn chained_transform_maps_drive_node_stack_traces_with_banner_and_minification() {
    let fixture = Fixture::new();
    let map = json!({
        "version": 3, "sources": ["original.ts"], "sourcesContent": ["original"],
        "names": ["fail"], "mappings": "AAwCGA;AACAA",
    })
    .to_string();
    let input = input_with_comment(
        "function fail() { throw new Error('mapped'); }\nfail();",
        &data_url(&map, true),
        false,
    );
    for format in [
        BuildFormat::Default,
        BuildFormat::CommonJs,
        BuildFormat::EsModule,
    ] {
        for minify in [false, true] {
            let options = TransformOptions {
                sourcefile: "input.js".into(),
                sourcemap: BuildSourceMap::InlineAndExternal,
                format,
                banner: "// prefix\r\n// prefix 2".into(),
                footer: "// footer".into(),
                minify_whitespace: minify,
                minify_identifiers: minify,
                minify_syntax: minify,
                ..TransformOptions::default()
            };
            let result = transform(&input, options.clone());
            assert!(result.errors.is_empty() && result.warnings.is_empty());
            node_stack(&result.code, &fixture.0.join("rust.cjs"));
            if let Some(go) = compare_go(&input, &options, &result) {
                node_stack(
                    go["Code"].as_str().unwrap().as_bytes(),
                    &fixture.0.join("go.cjs"),
                );
            }
        }
    }
}
