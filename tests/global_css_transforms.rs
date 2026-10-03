use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{
    BuildOptions, BuildResult, BuildSourceMap, BuildStdin, Loader, TransformOptions,
    TransformResult, build, transform,
};
use serde_json::{Value, json};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
static REPORT_LOCK: Mutex<()> = Mutex::new(());

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "esbuild-global-css-{}-{stamp}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
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

fn loader_number(loader: Loader) -> u8 {
    match loader {
        Loader::Css => 4,
        Loader::GlobalCss => 9,
        Loader::LocalCss => 13,
        _ => panic!("CSS loader"),
    }
}

fn flags(mask: u8) -> (bool, bool, bool) {
    (mask & 1 != 0, mask & 2 != 0, mask & 4 != 0)
}

fn go_call(request: &Value) -> Option<Value> {
    let executable = std::env::var_os("ESBUILD_RS_TEAM_GO_GLOBAL_CSS")?;
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
        .write_all(&serde_json::to_vec(request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let go: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(go["Errors"], Value::Null, "{request}\n{go}");
    assert_eq!(go["Warnings"], Value::Null, "{request}\n{go}");
    Some(go)
}

fn report(value: &Value) {
    if let Some(path) = std::env::var_os("ESBUILD_RS_TEAM_GLOBAL_CSS_REPORT") {
        let _guard = REPORT_LOCK.lock().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{value}").unwrap();
    }
}

fn compare_transform(input: &str, options: &TransformOptions, result: &TransformResult) {
    assert!(
        result.errors.is_empty() && result.warnings.is_empty(),
        "{options:?}\n{result:?}"
    );
    let request = json!({"Kind": "transform", "Input": input, "Transform": {
        "Loader": loader_number(options.loader), "Sourcefile": options.sourcefile,
        "MinifyWhitespace": options.minify_whitespace, "MinifyIdentifiers": options.minify_identifiers,
        "MinifySyntax": options.minify_syntax, "Sourcemap": if options.sourcemap == BuildSourceMap::None {0} else {3},
    }});
    let Some(go) = go_call(&request) else {
        return;
    };
    assert_eq!(
        go["Code"],
        String::from_utf8(result.code.clone()).unwrap(),
        "{request}"
    );
    let mut check =
        json!({"request": request, "code": go["Code"], "exactCodeAndDiagnostics": true});
    if options.sourcemap == BuildSourceMap::None {
        assert!(result.map.is_empty());
        assert_eq!(go["Map"], "");
    } else {
        let mut rust_map: Value = serde_json::from_slice(&result.map).unwrap();
        let mut go_map: Value = serde_json::from_str(go["Map"].as_str().unwrap()).unwrap();
        // The CSS parser's preexisting missing brace locations prevent full
        // stream parity. This test checks names and all other map metadata.
        check["rustMappings"] = rust_map
            .as_object_mut()
            .unwrap()
            .remove("mappings")
            .unwrap();
        check["goMappings"] = go_map.as_object_mut().unwrap().remove("mappings").unwrap();
        assert_eq!(rust_map, go_map, "{request}");
        check["exactMapMetadata"] = json!(true);
        check["metadata"] = rust_map;
    }
    report(&check);
}

fn compare_build(options: &BuildOptions, result: &BuildResult) {
    assert!(
        result.errors.is_empty() && result.warnings.is_empty(),
        "{options:?}\n{result:?}"
    );
    let loaders: HashMap<_, _> = options
        .loader
        .iter()
        .map(|(extension, loader)| (extension, loader_number(*loader)))
        .collect();
    let request = json!({"Kind": "build", "Build": {
        "AbsWorkingDir": options.abs_working_dir, "EntryPoints": options.entry_points,
        "Outfile": options.outfile, "Bundle": options.bundle, "Write": false, "Loader": loaders,
        "MinifyWhitespace": options.minify_whitespace, "MinifyIdentifiers": options.minify_identifiers,
        "MinifySyntax": options.minify_syntax,
        "Stdin": options.stdin.as_ref().map(|stdin| json!({"Contents": stdin.contents,
            "Sourcefile": stdin.sourcefile, "Loader": loader_number(stdin.loader)})),
    }});
    let Some(go) = go_call(&request) else {
        return;
    };
    let rust_files: BTreeMap<_, _> = result
        .output_files
        .iter()
        .map(|file| {
            (
                file.path.clone(),
                (file.contents.clone(), file.hash.clone()),
            )
        })
        .collect();
    let go_files: BTreeMap<_, _> = go["Files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["Path"].as_str().unwrap().to_owned(),
                (
                    STANDARD.decode(file["Contents"].as_str().unwrap()).unwrap(),
                    file["Hash"].as_str().unwrap().to_owned(),
                ),
            )
        })
        .collect();
    assert_eq!(rust_files, go_files, "{request}");
    report(&json!({"request": request, "exactFilesAndDiagnostics": true, "files": go["Files"]}));
}

#[test]
fn global_css_explicit_locals_are_renamed_and_plain_css_retains_annotations() {
    for (loader, expected) in [
        (Loader::Css, ":local(.local) {\n  color: red;\n}\n"),
        (Loader::GlobalCss, ".case_local {\n  color: red;\n}\n"),
        (Loader::LocalCss, ".case_local {\n  color: red;\n}\n"),
    ] {
        let options = TransformOptions {
            loader,
            sourcefile: "case.css".into(),
            ..TransformOptions::default()
        };
        let result = transform(":local(.local) {color:red}", options.clone());
        assert_eq!(result.code, expected.as_bytes());
        compare_transform(":local(.local) {color:red}", &options, &result);
    }
}

#[test]
fn annotated_selectors_keyframes_and_collisions_match_go_with_minification() {
    for (input, global_fragments) in [
        (
            ".top:local(.local):global(.shared) {color:red}",
            &[".top.case_local.shared"][..],
        ),
        (
            ":local { @keyframes spin {from {opacity:0} to {opacity:1}} .local {animation:spin 1s} } :global { @keyframes shared {to {opacity:1}} .shared {animation-name:shared} }",
            &[
                "@keyframes case_spin",
                "animation: case_spin 1s",
                "@keyframes shared",
                ".shared",
            ][..],
        ),
        (
            ":local(.local):global(.case_local):global(.case_local2) {color:red}",
            &[".case_local3.case_local.case_local2"][..],
        ),
        (
            ":local(#id.local), :global(.shared) {color:red} :local(.local) {color:blue}",
            &["#case_id.case_local", ".shared"][..],
        ),
        (
            "@keyframes spin {to {opacity:1}} .plain {animation-name:spin} :local(.local) {color:blue}",
            &["@keyframes spin", ".plain", ".case_local"][..],
        ),
    ] {
        for loader in [Loader::Css, Loader::GlobalCss, Loader::LocalCss] {
            for mask in 0..8 {
                let (minify_whitespace, minify_identifiers, minify_syntax) = flags(mask);
                let options = TransformOptions {
                    loader,
                    sourcefile: "case.css".into(),
                    minify_whitespace,
                    minify_identifiers,
                    minify_syntax,
                    ..TransformOptions::default()
                };
                let result = transform(input, options.clone());
                if loader == Loader::GlobalCss && mask == 0 {
                    let code = String::from_utf8_lossy(&result.code);
                    for fragment in global_fragments {
                        assert!(code.contains(fragment), "{code}");
                    }
                }
                compare_transform(input, &options, &result);
            }
        }
    }
    // Map generation already takes the linker route. Turning it on must keep
    // the exact same names and emitted CSS as the corrected direct route.
    for loader in [Loader::Css, Loader::GlobalCss, Loader::LocalCss] {
        for minify_identifiers in [false, true] {
            let input = ":local(.local):global(.case_local):global(.case_local2) {color:red}";
            let options = TransformOptions {
                loader,
                sourcefile: "case.css".into(),
                minify_identifiers,
                ..TransformOptions::default()
            };
            let direct = transform(input, options.clone());
            let mapped_options = TransformOptions {
                sourcemap: BuildSourceMap::External,
                ..options
            };
            let mapped = transform(input, mapped_options.clone());
            assert_eq!(direct.code, mapped.code);
            compare_transform(input, &mapped_options, &mapped);
        }
    }
}

#[test]
fn local_prefixes_follow_sourcefile_labels_and_global_names_remain_global() {
    for (sourcefile, prefix) in [
        ("", "stdin"),
        ("case.css", "case"),
        ("dir/case.module.css", "case"),
        ("/styles/name-with-dash.css", "name_with_dash"),
        ("C:\\styles\\case.css", "case"),
        ("https://example.com/case.css", "case"),
        ("<virtual>", "virtual"),
        ("/styles/index.css", "styles"),
        ("dir/123.css", "_"),
    ] {
        for loader in [Loader::GlobalCss, Loader::LocalCss] {
            for minify_identifiers in [false, true] {
                let input = ":local(.local):global(.shared) {color:red}";
                let options = TransformOptions {
                    loader,
                    sourcefile: sourcefile.into(),
                    minify_identifiers,
                    ..TransformOptions::default()
                };
                let result = transform(input, options.clone());
                assert!(String::from_utf8_lossy(&result.code).contains(".shared"));
                if !minify_identifiers {
                    assert!(
                        String::from_utf8_lossy(&result.code)
                            .contains(&format!(".{prefix}_local.shared"))
                    );
                }
                compare_transform(input, &options, &result);
            }
        }
    }
}

#[test]
fn stdin_and_multi_file_css_builds_preserve_linker_local_names() {
    let fixture = Fixture::new();
    for loader in [Loader::Css, Loader::GlobalCss, Loader::LocalCss] {
        for bundle in [false, true] {
            for mask in 0..8 {
                let (minify_whitespace, minify_identifiers, minify_syntax) = flags(mask);
                let options = BuildOptions {
                    abs_working_dir: fixture.0.to_string_lossy().into_owned(),
                    outfile: "out.css".into(),
                    bundle,
                    minify_whitespace,
                    minify_identifiers,
                    minify_syntax,
                    stdin: Some(BuildStdin {
                        loader,
                        sourcefile: "case.css".into(),
                        contents:
                            ":local(.local):global(.case_local):global(.case_local2) {color:red}"
                                .into(),
                        ..BuildStdin::default()
                    }),
                    ..BuildOptions::default()
                };
                let result = build(options.clone());
                if !minify_identifiers && loader != Loader::Css {
                    assert!(
                        String::from_utf8_lossy(&result.output_files[0].contents)
                            .contains(".case_local3.case_local.case_local2")
                    );
                }
                compare_build(&options, &result);
            }
        }
    }
    std::fs::create_dir_all(fixture.0.join("a")).unwrap();
    std::fs::create_dir_all(fixture.0.join("b")).unwrap();
    std::fs::write(
        fixture.0.join("entry.css"),
        "@import './a/case.css'; @import './b/case.css'; :global(.case_local) {color:green}",
    )
    .unwrap();
    std::fs::write(fixture.0.join("a/case.css"), ":local(.local) {color:red}").unwrap();
    std::fs::write(fixture.0.join("b/case.css"), ":local(.local) {color:blue}").unwrap();
    for loader in [Loader::Css, Loader::GlobalCss, Loader::LocalCss] {
        for minify in [false, true] {
            let options = BuildOptions {
                abs_working_dir: fixture.0.to_string_lossy().into_owned(),
                entry_points: vec!["entry.css".into()],
                loader: HashMap::from([(".css".into(), loader)]),
                outfile: "out.css".into(),
                bundle: true,
                minify_whitespace: minify,
                minify_identifiers: minify,
                minify_syntax: minify,
                ..BuildOptions::default()
            };
            compare_build(&options, &build(options.clone()));
        }
    }
}
