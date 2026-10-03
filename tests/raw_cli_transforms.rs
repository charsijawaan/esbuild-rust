//! CLI and formatted native transforms must deliver byte payloads to loaders.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{
    BuildFormat, Loader, Location, Message, TransformOptions, TransformResult, transform,
};
use serde_json::{Value, json};

const PAYLOADS: &[&[u8]] = &[&[0xff], &[0xff, 0], &[0xff, 0, 0xfe, b'A'], &[0], &[]];
const FORMATS: &[(BuildFormat, &str)] = &[
    (BuildFormat::CommonJs, "cjs"),
    (BuildFormat::EsModule, "esm"),
    (BuildFormat::Iife, "iife"),
];
const RAW_LOADERS: &[(Loader, &str)] = &[
    (Loader::Base64, "base64"),
    (Loader::Binary, "binary"),
    (Loader::DataUrl, "dataurl"),
    (Loader::File, "file"),
    (Loader::Copy, "copy"),
];

fn run(binary: &OsStr, arguments: &[String], input: &[u8], directory: Option<&Path>) -> Output {
    let mut command = Command::new(binary);
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn record(value: &Value) {
    static LOCK: Mutex<()> = Mutex::new(());
    let Some(path) = std::env::var_os("ESBUILD_RS_RAW_TRANSFORM_REPORT") else {
        return;
    };
    let _guard = LOCK.lock().unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    file.write_all(&bytes).unwrap();
}

fn location_value(location: &Location) -> Value {
    json!({
        "File": location.file, "Namespace": location.namespace, "Line": location.line,
        "Column": location.column, "Length": location.length, "LineText": location.line_text,
        "Suggestion": location.suggestion,
    })
}

fn messages_value(messages: &[Message]) -> Value {
    if messages.is_empty() {
        return Value::Null;
    }
    Value::Array(messages.iter().map(|message| json!({
        "ID": message.id, "PluginName": message.plugin_name, "Text": message.text,
        "Location": message.location.as_ref().map(location_value), "Detail": null,
        "Notes": if message.notes.is_empty() { Value::Null } else {
            Value::Array(message.notes.iter().map(|note| json!({
                "Text": note.text, "Location": note.location.as_ref().map(location_value),
            })).collect())
        },
    })).collect())
}

fn compare_go_transform(input: &[u8], options: &TransformOptions, result: &TransformResult) {
    let Some(binary) = std::env::var_os("ESBUILD_RS_RAW_TRANSFORM_GO_API") else {
        return;
    };
    let request = json!({
        "Input": STANDARD.encode(input), "Loader": options.loader as u16,
        "Format": options.format as u8, "Sourcefile": options.sourcefile,
        "GlobalName": options.global_name,
    });
    let output = run(&binary, &[], &serde_json::to_vec(&request).unwrap(), None);
    assert!(output.status.success(), "{output:?}");
    let reference: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        messages_value(&result.errors),
        reference["Errors"],
        "{request}"
    );
    assert_eq!(
        messages_value(&result.warnings),
        reference["Warnings"],
        "{request}"
    );
    for (name, bytes) in [
        ("Code", &result.code),
        ("Map", &result.map),
        ("LegalComments", &result.legal_comments),
    ] {
        let expected = STANDARD
            .decode(reference[name].as_str().unwrap_or_default())
            .unwrap();
        assert_eq!(*bytes, expected, "{name}: {request}");
    }
    assert_eq!(
        result.mangle_cache.is_some(),
        !reference["MangleCache"].is_null()
    );
    record(&json!({"kind": "transform", "request": request, "go": reference}));
}

fn cli_arguments(loader: &str, format: &str, bundle: bool) -> Vec<String> {
    let mut arguments = vec![
        format!("--loader={loader}"),
        format!("--format={format}"),
        "--global-name=RawPayload".into(),
        "--log-level=warning".into(),
        "--color=false".into(),
    ];
    if bundle {
        arguments.push("--bundle".into());
    }
    arguments
}

fn compare_go_cli(arguments: &[String], input: &[u8], result: &Output, success: bool) {
    let Some(binary) = std::env::var_os("ESBUILD_RS_RAW_TRANSFORM_GO_CLI") else {
        return;
    };
    let reference = run(&binary, arguments, input, None);
    assert_eq!(
        result.status.code(),
        reference.status.code(),
        "{arguments:?}"
    );
    assert_eq!(result.stdout, reference.stdout, "{arguments:?}: {input:?}");
    if success {
        assert_eq!(result.stderr, reference.stderr, "{arguments:?}: {input:?}");
    } else {
        // Public Rust locations hold String line text, while Go CLI excerpts
        // can contain invalid UTF-8. Compare the diagnostic header exactly and
        // retain the actual byte streams in the report instead of normalizing.
        assert_eq!(
            result.stderr.split(|byte| *byte == b'\n').next(),
            reference.stderr.split(|byte| *byte == b'\n').next(),
            "{arguments:?}"
        );
    }
    record(&json!({
        "kind": "cli", "arguments": arguments, "input": STANDARD.encode(input),
        "stdout": STANDARD.encode(&result.stdout), "goStdout": STANDARD.encode(&reference.stdout),
        "stderr": STANDARD.encode(&result.stderr), "goStderr": STANDARD.encode(&reference.stderr),
        "exactStderr": result.stderr == reference.stderr, "exit": result.status.code(),
    }));
}

fn assert_decoded_runtime(
    result: &TransformResult,
    loader: Loader,
    format: BuildFormat,
    input: &[u8],
) {
    if loader == Loader::Copy {
        assert_eq!(result.code, input);
        return;
    }
    let script = r"
        (async () => {
          const p = JSON.parse(process.argv[1]);
          let value;
          if (p.format === 2) {
            const m = {exports: {}};
            new Function('module', 'exports', p.code)(m, m.exports);
            value = m.exports;
          } else if (p.format === 3) {
            value = (await import('data:text/javascript;base64,' + Buffer.from(p.code).toString('base64'))).default;
          } else {
            value = new Function(p.code + '\nreturn RawPayload;')();
          }
          if (p.loader === 8) {
            if (typeof value !== 'string' || !value.startsWith('./input-') || !value.endsWith('.bin')) throw Error(value);
            console.log('asset');
          } else {
            let bytes;
            if (p.loader === 1) bytes = Buffer.from(value, 'base64');
            else if (p.loader === 2) bytes = value;
            else {
              const comma = value.indexOf(',');
              const text = value.slice(comma + 1);
              bytes = value.slice(0, comma).endsWith(';base64') ? Buffer.from(text, 'base64') :
                Buffer.from(text.replace(/%([0-9a-f]{2})/gi, (_, hex) => String.fromCharCode(parseInt(hex, 16))), 'latin1');
            }
            console.log(JSON.stringify([...bytes]));
          }
        })().catch(error => { console.error(error); process.exit(1); });
    ";
    let request = json!({"code": String::from_utf8(result.code.clone()).unwrap(),
        "loader": loader as u16, "format": format as u8});
    let output = Command::new("node")
        .args(["-e", script, &request.to_string()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{request}: {output:?}");
    if loader == Loader::File {
        assert_eq!(output.stdout, b"asset\n");
    } else {
        assert_eq!(
            serde_json::from_slice::<Vec<u8>>(&output.stdout).unwrap(),
            input
        );
    }
}

#[test]
fn formatted_raw_loader_outputs_preserve_bytes_and_select_primary_output() {
    for &(loader, _) in RAW_LOADERS {
        for &(format, _) in FORMATS {
            for &input in PAYLOADS {
                let options = TransformOptions {
                    loader,
                    format,
                    sourcefile: "input.bin".into(),
                    global_name: "RawPayload".into(),
                    ..TransformOptions::default()
                };
                let result = transform(input, options.clone());
                assert!(
                    result.errors.is_empty(),
                    "{loader:?} {format:?} {input:?}: {:?}",
                    result.errors
                );
                assert!(result.warnings.is_empty());
                compare_go_transform(input, &options, &result);
                assert_decoded_runtime(&result, loader, format, input);
            }
        }
    }
}

#[test]
fn cli_formatted_and_bundled_raw_stdin_preserves_invalid_nul_and_empty_bytes() {
    for &(loader, name) in &RAW_LOADERS[..3] {
        for &(_, format) in FORMATS {
            for bundle in [false, true] {
                for &input in PAYLOADS {
                    let arguments = cli_arguments(name, format, bundle);
                    let result = run(
                        OsStr::new(env!("CARGO_BIN_EXE_esbuild")),
                        &arguments,
                        input,
                        None,
                    );
                    assert!(
                        result.status.success(),
                        "{loader:?} {format} {bundle}: {result:?}"
                    );
                    assert!(result.stderr.is_empty(), "{result:?}");
                    assert!(!result.stdout.is_empty());
                    compare_go_cli(&arguments, input, &result, true);
                }
            }
        }
    }
}

#[test]
fn original_base64_stdin_reproducers_match_exact_go_output() {
    for (bundle, expected) in [
        (false, &b"module.exports = \"/wA=\";\n"[..]),
        (true, &b"// <stdin>\nvar stdin_default = \"/wA=\";\n"[..]),
    ] {
        let arguments = cli_arguments("base64", "cjs", bundle);
        let result = run(
            OsStr::new(env!("CARGO_BIN_EXE_esbuild")),
            &arguments,
            &[0xff, 0],
            None,
        );
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, expected);
        compare_go_cli(&arguments, &[0xff, 0], &result, true);
    }
}

#[test]
fn formatted_javascript_invalid_bytes_are_diagnosed_by_the_lexer() {
    for loader in [Loader::Js, Loader::Jsx, Loader::Ts, Loader::Tsx] {
        for &(format, _) in FORMATS {
            for input in [&[0xff][..], &[0xff, 0], &[0]] {
                let options = TransformOptions {
                    loader,
                    format,
                    sourcefile: "input.js".into(),
                    ..TransformOptions::default()
                };
                let result = transform(input, options.clone());
                assert!(result.code.is_empty());
                assert_eq!(result.errors.len(), 1);
                assert_eq!(
                    result.errors[0].text,
                    if input[0] == 0xff {
                        "Unexpected \"\\xff\""
                    } else {
                        "Unexpected \"\\x00\""
                    }
                );
                let location = result.errors[0].location.as_ref().unwrap();
                assert_eq!(location.file, "input.js");
                assert_eq!((location.line, location.column, location.length), (1, 0, 1));
                compare_go_transform(input, &options, &result);
            }
        }
    }
}

#[test]
fn cli_invalid_javascript_bytes_reach_the_lexer_in_both_stdin_paths() {
    for loader in ["js", "ts"] {
        for &(_, format) in FORMATS {
            for bundle in [false, true] {
                for input in [&[0xff][..], &[0xff, 0], &[0]] {
                    let arguments = cli_arguments(loader, format, bundle);
                    let result = run(
                        OsStr::new(env!("CARGO_BIN_EXE_esbuild")),
                        &arguments,
                        input,
                        None,
                    );
                    assert!(!result.status.success());
                    assert!(result.stdout.is_empty());
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    assert!(
                        stderr.contains(if input[0] == 0xff {
                            "Unexpected \"\\xff\""
                        } else {
                            "Unexpected \"\\x00\""
                        }),
                        "{stderr}"
                    );
                    assert!(!stderr.contains("must be valid UTF-8"));
                    assert!(!stderr.contains("panicked"));
                    compare_go_cli(&arguments, input, &result, false);
                }
            }
        }
    }
}

#[test]
fn text_transform_api_remains_backward_compatible() {
    for &(loader, _) in &RAW_LOADERS[..3] {
        for &(format, _) in FORMATS {
            let options = TransformOptions {
                loader,
                format,
                ..TransformOptions::default()
            };
            let text = "ÿ\0";
            let string_result = transform(text, options.clone());
            let byte_result = transform(text.as_bytes(), options.clone());
            assert!(string_result.errors.is_empty());
            assert_eq!(string_result.code, byte_result.code);
            compare_go_transform(text.as_bytes(), &options, &string_result);
        }
    }
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "esbuild-raw-cli-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn output_files(path: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_str().unwrap().to_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn cli_file_and_copy_loaders_preserve_asset_bytes_when_selected_by_extension() {
    for loader in ["file", "copy"] {
        for &input in PAYLOADS {
            let fixture = Fixture::new();
            std::fs::write(fixture.0.join("input.bin"), input).unwrap();
            let mut arguments = vec![
                "input.bin".into(),
                format!("--loader:.bin={loader}"),
                "--bundle".into(),
                "--format=cjs".into(),
                "--log-level=warning".into(),
                "--outdir=rust-out".into(),
            ];
            let result = run(
                OsStr::new(env!("CARGO_BIN_EXE_esbuild")),
                &arguments,
                &[],
                Some(&fixture.0),
            );
            assert!(result.status.success(), "{result:?}");
            let actual = output_files(&fixture.0.join("rust-out"));
            assert!(actual.iter().any(|(name, bytes)| {
                Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
                    && bytes == input
            }));
            if let Some(binary) = std::env::var_os("ESBUILD_RS_RAW_TRANSFORM_GO_CLI") {
                *arguments.last_mut().unwrap() = "--outdir=go-out".into();
                let reference = run(&binary, &arguments, &[], Some(&fixture.0));
                assert!(reference.status.success(), "{reference:?}");
                assert_eq!(result.stdout, reference.stdout);
                assert_eq!(result.stderr, reference.stderr);
                assert_eq!(actual, output_files(&fixture.0.join("go-out")));
                record(
                    &json!({"kind": "cli-assets", "loader": loader, "input": STANDARD.encode(input), "files": actual.keys().collect::<Vec<_>>() }),
                );
            }
        }
    }
}

#[test]
fn cli_file_and_copy_stdin_restrictions_precede_loading_raw_bytes() {
    for loader in ["file", "copy"] {
        for bundle in [false, true] {
            for input in [&[0xff, 0][..], &[]] {
                let arguments = cli_arguments(loader, "cjs", bundle);
                let result = run(
                    OsStr::new(env!("CARGO_BIN_EXE_esbuild")),
                    &arguments,
                    input,
                    None,
                );
                assert!(!result.status.success());
                assert!(result.stdout.is_empty());
                let stderr = String::from_utf8_lossy(&result.stderr);
                let diagnostic =
                    format!("\"--loader={loader}\" is not supported when transforming stdin");
                assert!(stderr.contains(&diagnostic), "{stderr}");
                assert!(!stderr.contains("must be valid UTF-8"));
                if let Some(binary) = std::env::var_os("ESBUILD_RS_RAW_TRANSFORM_GO_CLI") {
                    let reference = run(&binary, &arguments, input, None);
                    assert_eq!(result.status.code(), reference.status.code());
                    assert!(String::from_utf8_lossy(&reference.stderr).contains(&diagnostic));
                    record(&json!({"kind": "cli-unsupported-stdin", "loader": loader,
                        "input": STANDARD.encode(input), "bundle": bundle,
                        "stderr": STANDARD.encode(&result.stderr), "goStderr": STANDARD.encode(&reference.stderr),
                        "exactStderr": result.stderr == reference.stderr,
                    }));
                }
            }
        }
    }
}
