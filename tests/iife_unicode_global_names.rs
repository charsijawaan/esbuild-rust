use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{BuildFormat, BuildSourceMap, Target, TransformOptions, transform};
use esbuild_rs::internal::{
    helpers::string_to_utf16,
    js_parser::parse_global_name,
    logger::{DeferLogKind, Log, Source},
};

const SOURCE: &str = "export default 123";

fn options_for(name: &str, ascii_only: bool, logical_assignment: bool) -> TransformOptions {
    TransformOptions {
        format: BuildFormat::Iife,
        global_name: name.into(),
        ascii_only,
        supported: HashMap::from([("logical-assignment".into(), logical_assignment)]),
        ..TransformOptions::default()
    }
}

fn run_with_stdin(mut command: Command, contents: &str) -> String {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run compiler or Node.js");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(contents.as_bytes())
        .expect("write child stdin");
    let output = child.wait_with_output().expect("read child output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("child output is UTF-8")
}

fn go_output(source: &str, options: &TransformOptions) -> Option<String> {
    let executable = std::env::var_os("ESBUILD_RS_TEAM_IIFE_UNICODE_GO")?;
    let mut command = Command::new(executable);
    command.args([
        "--format=iife",
        &format!("--global-name={}", options.global_name),
        if options.ascii_only {
            "--charset=ascii"
        } else {
            "--charset=utf8"
        },
    ]);
    match options.target {
        Target::Default => {}
        Target::Es2015 => {
            command.arg("--target=es2015");
        }
        Target::Es2021 => {
            command.arg("--target=es2021");
        }
        _ => panic!("unexpected reference target"),
    }
    for (feature, supported) in &options.supported {
        command.arg(format!("--supported:{feature}={supported}"));
    }
    if options.minify_whitespace {
        command.arg("--minify-whitespace");
    }
    if options.sourcemap != BuildSourceMap::None {
        command.args([
            "--sourcemap=inline",
            &format!("--sourcefile={}", options.sourcefile),
        ]);
    }
    Some(run_with_stdin(command, source))
}

fn transform_code(options: &TransformOptions) -> String {
    let result = transform(SOURCE, options.clone());
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    let code = String::from_utf8(result.code).expect("Rust JavaScript");
    if let Some(reference) = go_output(SOURCE, options) {
        // Preserve the original four tests' exact prefix assertion scope.
        assert_eq!(
            prefix(&code, options),
            prefix(&reference, options),
            "{options:?}"
        );
    }
    code
}

fn prefix<'a>(code: &'a str, options: &TransformOptions) -> &'a str {
    let start = if options.minify_whitespace {
        "(()=>{"
    } else {
        "(() => {\n"
    };
    &code[..code.find(start).expect("IIFE wrapper")]
}

fn runtime_record(code: &str, name: &str) -> serde_json::Value {
    let log = Log::new_defer(DeferLogKind::All, HashMap::new());
    let (parts, ok) = parse_global_name(
        log.clone(),
        Source {
            contents: Arc::from(name.as_bytes()),
            ..Source::default()
        },
    );
    assert!(ok);
    assert!(log.done().is_empty());
    let parts: Vec<_> = parts.iter().map(|part| string_to_utf16(part)).collect();
    serde_json::json!({ "code": code, "parts": parts })
}

fn execute_records(records: &[serde_json::Value]) {
    let program = format!(
        r"const assert = require('node:assert/strict');
const vm = require('node:vm');
for (const {{ code, parts }} of {}) {{
  const globals = {{}};
  vm.runInNewContext(code, globals);
  const names = parts.map(units => String.fromCharCode(...units));
  if (names[0] === 'this') names.shift();
  let value = globals;
  for (const name of names) value = value[name];
  assert.equal(value.default, 123, code);
}}
console.log('ok');",
        serde_json::to_string(records).expect("serialize runtime cases")
    );
    assert_eq!(run_with_stdin(Command::new("node"), &program), "ok\n");
}

#[test]
fn unicode_compound_global_names_match_all_four_original_prefixes() {
    let name = "π[\"π 𐀀\"].𐀀[\"𐀀 π\"]";
    let mut records = Vec::new();
    for (ascii_only, logical_assignment, expected) in [
        (
            true,
            true,
            "var \\u03C0;\n(((\\u03C0 ||= {})[\"\\u03C0 \\uD800\\uDC00\"] ||= {})[\"\\uD800\\uDC00\"] ||= {})[\"\\uD800\\uDC00 \\u03C0\"] = ",
        ),
        (
            false,
            true,
            "var π;\n(((π ||= {})[\"π 𐀀\"] ||= {})[\"𐀀\"] ||= {})[\"𐀀 π\"] = ",
        ),
        (
            true,
            false,
            "var \\u03C0 = \\u03C0 || {};\n\\u03C0[\"\\u03C0 \\uD800\\uDC00\"] = \\u03C0[\"\\u03C0 \\uD800\\uDC00\"] || {};\n\\u03C0[\"\\u03C0 \\uD800\\uDC00\"][\"\\uD800\\uDC00\"] = \\u03C0[\"\\u03C0 \\uD800\\uDC00\"][\"\\uD800\\uDC00\"] || {};\n\\u03C0[\"\\u03C0 \\uD800\\uDC00\"][\"\\uD800\\uDC00\"][\"\\uD800\\uDC00 \\u03C0\"] = ",
        ),
        (
            false,
            false,
            "var π = π || {};\nπ[\"π 𐀀\"] = π[\"π 𐀀\"] || {};\nπ[\"π 𐀀\"][\"𐀀\"] = π[\"π 𐀀\"][\"𐀀\"] || {};\nπ[\"π 𐀀\"][\"𐀀\"][\"𐀀 π\"] = ",
        ),
    ] {
        let options = options_for(name, ascii_only, logical_assignment);
        let code = transform_code(&options);
        assert_eq!(prefix(&code, &options), expected);
        records.push(runtime_record(&code, name));
    }
    execute_records(&records);
}

#[test]
fn global_name_identifiers_follow_es5_rules_with_charset_and_target_overrides() {
    let cases = [
        ("Bundle.A", ".A", ".A"),
        ("Bundle.π", ".\\u03C0", ".π"),
        ("Bundle.Ƞ", "[\"\\u0220\"]", "[\"Ƞ\"]"),
        ("Bundle.𐀀", "[\"\\uD800\\uDC00\"]", "[\"𐀀\"]"),
        ("𐀀.π", "this[\"\\uD800\\uDC00\"]", "this[\"𐀀\"]"),
        ("Bundle.default", ".default", ".default"),
        (
            r#"Bundle["\uD800\uDC00"]"#,
            "[\"\\uD800\\uDC00\"]",
            "[\"𐀀\"]",
        ),
    ];
    let mut records = Vec::new();
    for target in [Target::Es2015, Target::Es2021] {
        for ascii_only in [false, true] {
            for logical_assignment in [None, Some(false), Some(true)] {
                for unicode_escapes in [false, true] {
                    for (name, ascii, utf8) in cases {
                        let mut options = options_for(name, ascii_only, true);
                        options.target = target;
                        options.supported.clear();
                        if let Some(supported) = logical_assignment {
                            options
                                .supported
                                .insert("logical-assignment".into(), supported);
                        }
                        options
                            .supported
                            .insert("unicode-escapes".into(), unicode_escapes);
                        let code = transform_code(&options);
                        let prefix = prefix(&code, &options);
                        assert!(
                            prefix.contains(if ascii_only { ascii } else { utf8 }),
                            "{prefix}"
                        );
                        let logical = logical_assignment.unwrap_or(target == Target::Es2021);
                        assert_eq!(prefix.contains("||="), logical, "{prefix}");
                        records.push(runtime_record(&code, name));
                    }
                }
            }
        }
    }
    execute_records(&records);
}

#[test]
fn utf16_property_code_units_survive_global_name_parsing_and_public_transforms() {
    let mut records = Vec::new();
    for (name, accessor) in [
        (r#"Bundle["\uD800"]"#, r#"["\uD800"]"#),
        (r#"Bundle["\uDC00"]"#, r#"["\uDC00"]"#),
        (r#"Bundle["\uD800\uD800"]"#, r#"["\uD800\uD800"]"#),
        (r#"Bundle["\uDC00\uD800"]"#, r#"["\uDC00\uD800"]"#),
    ] {
        for ascii_only in [false, true] {
            for logical_assignment in [false, true] {
                for minify_whitespace in [false, true] {
                    let mut options = options_for(name, ascii_only, logical_assignment);
                    options.minify_whitespace = minify_whitespace;
                    let code = transform_code(&options);
                    assert!(prefix(&code, &options).contains(accessor), "{code}");
                    records.push(runtime_record(&code, name));
                }
            }
        }
    }
    execute_records(&records);
}

#[test]
fn unicode_global_name_prefixes_preserve_exact_source_maps() {
    let source = "console.log(\"π 𐀀\")";
    for ascii_only in [false, true] {
        for logical_assignment in [false, true] {
            for minify_whitespace in [false, true] {
                let mut options = options_for("π.𐀀.value", ascii_only, logical_assignment);
                options.minify_whitespace = minify_whitespace;
                options.sourcefile = "unicode-input.js".into();
                options.sourcemap = BuildSourceMap::External;
                let result = transform(source, options.clone());
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                assert!(result.warnings.is_empty(), "{:?}", result.warnings);
                let map: serde_json::Value = serde_json::from_slice(&result.map).expect("Rust map");
                assert_eq!(map["sources"], serde_json::json!(["unicode-input.js"]));
                assert_eq!(map["sourcesContent"], serde_json::json!([source]));
                if let Some(reference) = go_output(source, &options) {
                    let (code, encoded) = reference
                        .split_once("//# sourceMappingURL=data:application/json;base64,")
                        .expect("Go inline source map");
                    assert_eq!(result.code, code.as_bytes(), "{options:?}");
                    let bytes = STANDARD.decode(encoded.trim()).expect("decode Go map");
                    let reference_map: serde_json::Value =
                        serde_json::from_slice(&bytes).expect("Go map");
                    assert_eq!(map, reference_map, "{options:?}");
                }
            }
        }
    }
}
