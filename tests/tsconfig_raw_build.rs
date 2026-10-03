use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildResult, BuildStdin, Loader, LogLevel, Message,
    TransformOptions, build, transform,
};
use serde_json::{Value, json};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-tsconfig-raw-build-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        for (path, contents) in files {
            let path = directory.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        Self(std::fs::canonicalize(directory).unwrap())
    }

    fn options(&self, raw: &str) -> BuildOptions {
        BuildOptions {
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            bundle: true,
            outfile: "out.js".into(),
            format: BuildFormat::EsModule,
            tsconfig_raw: raw.into(),
            ..BuildOptions::default()
        }
    }

    fn build(options: BuildOptions) -> BuildResult {
        let request = json!({
            "mode": "build",
            "directory": options.abs_working_dir,
            "entryPoints": options.entry_points,
            "inject": options.inject,
            "raw": options.tsconfig_raw,
            "stdin": options.stdin.as_ref().map(|stdin| json!({
                "contents": stdin.contents,
                "resolveDir": stdin.resolve_dir,
                "sourcefile": stdin.sourcefile,
            })),
        });
        let result = build(options);
        compare_go(
            &request,
            &result.errors,
            &result.warnings,
            result
                .output_files
                .iter()
                .map(|file| file.contents.as_slice()),
        );
        result
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// The optional reference executable calls the unchanged pinned Go native API.
// Ordinary regression runs do not require Go or a separate checkout.
fn compare_go<'a>(
    request: &Value,
    errors: &[Message],
    warnings: &[Message],
    outputs: impl Iterator<Item = &'a [u8]>,
) {
    let Some(executable) = std::env::var_os("ESBUILD_RS_TSCONFIG_RAW_GO") else {
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
    let reference: Value = serde_json::from_slice(&reference.stdout).unwrap();
    for (name, actual) in [("errors", errors), ("warnings", warnings)] {
        let expected = reference[name].as_array().map_or(&[][..], Vec::as_slice);
        assert_eq!(actual.len(), expected.len(), "{request}\n{reference}");
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.text, expected["Text"].as_str().unwrap());
            assert_eq!(actual.id, expected["ID"].as_str().unwrap());
            if let Some(location) = &actual.location {
                let expected = &expected["Location"];
                assert_eq!(location.file, expected["File"].as_str().unwrap());
                assert_eq!(location.line as u64, expected["Line"].as_u64().unwrap());
                assert_eq!(location.column as u64, expected["Column"].as_u64().unwrap());
            }
        }
    }
    let expected = reference["outputs"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let actual: Vec<_> = outputs.collect();
    assert_eq!(actual.len(), expected.len(), "{request}\n{reference}");
    for (actual, expected) in actual.iter().zip(expected) {
        let expected = STANDARD.decode(expected.as_str().unwrap()).unwrap();
        assert_eq!(
            *actual, expected,
            "output differs from pinned Go: {request}"
        );
    }
}

fn output(result: &BuildResult) -> String {
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.output_files.len(), 1);
    String::from_utf8(result.output_files[0].contents.clone()).unwrap()
}

#[test]
fn raw_base_url_and_paths_use_build_cwd_and_replace_discovered_configs() {
    let fixture = Fixture::new(&[
        ("a/b/c/entry.js", "import 'test'; import 'bare';"),
        ("a/b/test-impl.js", "console.log('raw paths');"),
        ("a/b/bare.js", "console.log('raw base url');"),
        ("a/tsconfig.json", "FAILURE"),
        (
            "a/b/c/test-impl.js",
            "console.log('wrong source directory');",
        ),
    ]);
    let mut options = fixture.options(
        r#"{"compilerOptions":{"baseUrl":"./a/b","paths":{"test":["./missing.js","./test-impl.js"]}}}"#,
    );
    options.entry_points = vec!["a/b/c/entry.js".into()];
    let result = Fixture::build(options);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    let code = output(&result);
    assert!(
        code.contains("raw paths") && code.contains("raw base url"),
        "{code}"
    );
    assert!(!code.contains("wrong source directory"), "{code}");
}

#[test]
fn raw_paths_without_base_url_use_cwd_for_nested_stdin_resolve_directories() {
    let fixture = Fixture::new(&[
        ("test-impl.js", "console.log('raw stdin paths');"),
        (
            "sub/test-impl.js",
            "console.log('wrong resolve directory');",
        ),
        ("sub/tsconfig.json", "FAILURE"),
    ]);
    let mut options =
        fixture.options(r#"{"compilerOptions":{"paths":{"test":["./test-impl.js"]}}}"#);
    options.stdin = Some(BuildStdin {
        contents: "import 'test';".into(),
        resolve_dir: fixture.0.join("sub").to_string_lossy().into_owned(),
        sourcefile: "nested/input.js".into(),
        ..BuildStdin::default()
    });
    let result = Fixture::build(options);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    let code = output(&result);
    assert!(code.contains("raw stdin paths"), "{code}");
    assert!(!code.contains("wrong resolve directory"), "{code}");
}

#[test]
fn raw_path_remapping_is_excluded_from_dependency_imports() {
    let fixture = Fixture::new(&[
        ("entry.js", "import 'pkg';"),
        ("node_modules/pkg/index.js", "import 'alias';"),
        (
            "node_modules/alias/index.js",
            "console.log('dependency module');",
        ),
        ("app.js", "console.log('wrong raw remapping');"),
    ]);
    let mut options = fixture.options(r#"{"compilerOptions":{"paths":{"alias":["./app.js"]}}}"#);
    options.entry_points = vec!["entry.js".into()];
    let result = Fixture::build(options);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    let code = output(&result);
    assert!(code.contains("dependency module"), "{code}");
    assert!(!code.contains("wrong raw remapping"), "{code}");
}

#[test]
fn raw_config_validation_is_shared_by_injected_inputs_and_repeated_builds() {
    let fixture = Fixture::new(&[
        ("entry.js", "import 'alias';"),
        ("inject.js", "import 'alias';"),
        ("impl.js", "console.log('shared raw config');"),
    ]);
    let mut options = fixture
        .options(r#"{"compilerOptions":{"paths":{"alias":["./impl.js"],"unused":["bad.js"]}}}"#);
    options.inject = vec!["inject.js".into()];
    for use_stdin in [false, true, false] {
        options.entry_points = if use_stdin {
            Vec::new()
        } else {
            vec!["entry.js".into()]
        };
        options.stdin = use_stdin.then(|| BuildStdin {
            contents: "import 'alias';".into(),
            resolve_dir: fixture.0.to_string_lossy().into_owned(),
            ..BuildStdin::default()
        });
        let result = Fixture::build(options.clone());
        assert!(output(&result).contains("shared raw config"));
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert_eq!(result.warnings[0].id, "tsconfig.json");
        assert_eq!(
            result.warnings[0].location.as_ref().unwrap().file,
            "<tsconfig.json>"
        );
    }
}

#[test]
fn transform_raw_config_ignores_filesystem_extends_paths_and_discovery() {
    let fixture = Fixture::new(&[
        (
            "base.json",
            r#"{"compilerOptions":{"jsxFactory":"wrongInheritedFactory"}}"#,
        ),
        ("invalid.json", "FAILURE"),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"jsxFactory":"wrongDiscoveredFactory"}}"#,
        ),
        ("impl.js", "FAILURE"),
    ]);
    for extends in ["base.json", "invalid.json", "missing.json"] {
        let raw = json!({
            "extends": fixture.0.join(extends),
            "compilerOptions": {
                "baseUrl": fixture.0,
                "paths": {"alias": ["./impl.js"]},
            },
        })
        .to_string();
        let input = "import 'alias'; console.log(<div/>);";
        let sourcefile = fixture.0.join("input.jsx").to_string_lossy().into_owned();
        let result = transform(
            input,
            TransformOptions {
                loader: Loader::Jsx,
                sourcefile: sourcefile.clone(),
                tsconfig_raw: raw.clone(),
                log_override: HashMap::from([("tsconfig.json".into(), LogLevel::Error)]),
                ..TransformOptions::default()
            },
        );
        compare_go(
            &json!({
                "mode": "transform", "input": input, "raw": raw, "sourcefile": sourcefile,
            }),
            &result.errors,
            &result.warnings,
            std::iter::once(result.code.as_slice()),
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(
            String::from_utf8(result.code).unwrap(),
            "import \"alias\";\nconsole.log(/* @__PURE__ */ React.createElement(\"div\", null));\n"
        );
    }
}
