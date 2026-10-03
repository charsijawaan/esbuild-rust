use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{
    BuildEntryPoint, BuildFormat, BuildOptions, BuildResult, Loader, Message, OnLoadOptions,
    OnLoadResult, OnResolveOptions, OnResolveResult, Plugin, ResolveKind, build,
};
use serde_json::{Value, json};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-virtual-entry-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        for (path, contents) in files {
            let path = directory.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        Self(std::fs::canonicalize(directory).unwrap())
    }

    fn run(&self, entries: &[&str], advanced: &[(&str, &str)], plugin: bool) -> BuildResult {
        let resolves = Arc::new(Mutex::new(Vec::new()));
        let loads = Arc::new(Mutex::new(Vec::new()));
        let plugins = if plugin {
            vec![virtual_plugin(&resolves, &loads)]
        } else {
            Vec::new()
        };
        let result = build(BuildOptions {
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            // Pin the output base for filesystem glob controls so this case
            // tests matching and sanitization independently of base inference.
            outbase: if plugin { String::new() } else { ".".into() },
            entry_points: entries.iter().map(|entry| (*entry).into()).collect(),
            entry_points_advanced: advanced
                .iter()
                .map(|(input, output)| BuildEntryPoint {
                    input_path: (*input).into(),
                    output_path: (*output).into(),
                })
                .collect(),
            plugins,
            bundle: true,
            outdir: "out".into(),
            format: BuildFormat::EsModule,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let mut resolves = resolves.lock().unwrap().clone();
        resolves.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        let mut loads = loads.lock().unwrap().clone();
        loads.sort();
        compare_go(
            &json!({
                "directory": self.0,
                "entries": entries,
                "advanced": advanced.iter().map(|(input, output)| json!({
                    "InputPath": input, "OutputPath": output,
                })).collect::<Vec<_>>(),
                "plugin": plugin,
                "outbase": if plugin { "" } else { "." },
            }),
            &result,
            &resolves,
            &loads,
        );
        if plugin {
            assert_eq!(resolves.len(), entries.len() + advanced.len());
            assert_eq!(loads.len(), entries.len() + advanced.len());
            for call in &resolves {
                assert_eq!(call["importer"], "");
                assert_eq!(call["resolveDir"], self.0.to_string_lossy().as_ref());
            }
        }
        result
    }

    fn assert_paths(&self, result: &BuildResult, paths: &[&str]) {
        let actual = result
            .output_files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let expected = paths
            .iter()
            .map(|path| self.0.join("out").join(path).to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn virtual_plugin(resolves: &Arc<Mutex<Vec<Value>>>, loads: &Arc<Mutex<Vec<String>>>) -> Plugin {
    let resolves = resolves.clone();
    let loads = loads.clone();
    Plugin::new("virtual-entry", move |build| {
        build.on_resolve(
            OnResolveOptions {
                filter: ".*".into(),
                ..OnResolveOptions::default()
            },
            {
                let resolves = resolves.clone();
                move |args| {
                    assert_eq!(args.kind, ResolveKind::EntryPoint);
                    resolves.lock().unwrap().push(json!({
                        "path": args.path,
                        "namespace": args.namespace,
                        "importer": args.importer,
                        "resolveDir": args.resolve_dir,
                        "kind": 1,
                    }));
                    Ok(OnResolveResult {
                        path: format!("input {}", args.path),
                        namespace: "virtual-ns".into(),
                        ..OnResolveResult::default()
                    })
                }
            },
        );
        build.on_load(
            OnLoadOptions {
                filter: ".*".into(),
                namespace: "virtual-ns".into(),
            },
            {
                let loads = loads.clone();
                move |args| {
                    loads.lock().unwrap().push(args.path.clone());
                    Ok(OnLoadResult {
                        contents: Some(format!(
                            "console.log({})",
                            serde_json::to_string(&args.path).unwrap()
                        )),
                        loader: Loader::Js,
                        ..OnLoadResult::default()
                    })
                }
            },
        );
        Ok(())
    })
}

fn messages(messages: &[Message]) -> Value {
    json!(
        messages
            .iter()
            .map(|message| message.text.clone())
            .collect::<Vec<_>>()
    )
}

// The optional executable directly calls the unchanged pinned Go Build API.
fn compare_go(request: &Value, actual: &BuildResult, resolves: &[Value], loads: &[String]) {
    let Some(executable) = std::env::var_os("ESBUILD_RS_VIRTUAL_ENTRY_GO") else {
        return;
    };
    let mut process = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(request).unwrap())
        .unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    for (name, actual) in [("errors", &actual.errors), ("warnings", &actual.warnings)] {
        let texts = expected[name]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|message| message["Text"].clone())
            .collect::<Vec<_>>();
        assert_eq!(messages(actual), json!(texts), "{request}");
    }
    let expected_resolves = expected["resolves"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let expected_loads = expected["loads"].as_array().map_or(&[][..], Vec::as_slice);
    assert_eq!(resolves, expected_resolves, "{request}");
    assert_eq!(json!(loads), json!(expected_loads), "{request}");
    let outputs = expected["outputs"].as_array().unwrap();
    assert_eq!(actual.output_files.len(), outputs.len(), "{request}");
    for (actual, expected) in actual.output_files.iter().zip(outputs) {
        assert_eq!(actual.path, expected["path"].as_str().unwrap(), "{request}");
        let bytes = STANDARD
            .decode(expected["contents"].as_str().unwrap())
            .unwrap();
        assert_eq!(actual.contents, bytes, "{request}");
    }
}

#[test]
fn original_virtual_entries_reach_plugins_and_keep_sanitized_original_names() {
    let fixture = Fixture::new(&[]);
    let result = fixture.run(&["1", "2", "a<>:\"|?b", "a/b/c.d.e"], &[], true);
    fixture.assert_paths(&result, &["1.js", "2.js", "a_b.js", "a/b/c.d.js"]);
    let expected = [
        "// virtual-ns:input 1\nconsole.log(\"input 1\");\n",
        "// virtual-ns:input 2\nconsole.log(\"input 2\");\n",
        "// virtual-ns:input a<>:\"|?b\nconsole.log('input a<>:\"|?b');\n",
        "// virtual-ns:input a/b/c.d.e\nconsole.log(\"input a/b/c.d.e\");\n",
    ];
    for (file, expected) in result.output_files.iter().zip(expected) {
        assert_eq!(file.contents, expected.as_bytes());
    }
}

#[test]
fn literal_question_marks_and_invalid_name_runs_remain_virtual_entries() {
    let fixture = Fixture::new(&[]);
    let result = fixture.run(&["?", "<>?name?|", "dir?/leaf?name.js"], &[], true);
    fixture.assert_paths(&result, &["_.js", "name.js", "dir_/leaf_name.js"]);
}

#[test]
fn explicit_output_names_are_preserved_for_virtual_entries() {
    let fixture = Fixture::new(&[]);
    let result = fixture.run(&[], &[("virtual?input", "custom/named.result")], true);
    fixture.assert_paths(&result, &["custom/named.result.js"]);
}

#[test]
fn literal_question_mark_files_keep_file_namespace_and_plugin_rewrite() {
    let fixture = Fixture::new(&[("real?entry.js", "throw 'plugin should replace this';")]);
    let result = fixture.run(&["real?entry.js"], &[], true);
    fixture.assert_paths(&result, &["real_entry.js"]);
    assert!(
        String::from_utf8_lossy(&result.output_files[0].contents).contains("input ./real?entry.js")
    );
}

fn assert_prefixed_file_rewrite(input: &str, filename: &str, outbase: &str, output: &str) {
    let fixture = Fixture::new(&[(filename, "throw 'plugin should replace this';")]);
    let resolves = Arc::new(Mutex::new(Vec::new()));
    let loads = Arc::new(Mutex::new(Vec::new()));
    let result = build(BuildOptions {
        abs_working_dir: fixture.0.to_string_lossy().into_owned(),
        entry_points: vec![input.into()],
        plugins: vec![virtual_plugin(&resolves, &loads)],
        bundle: true,
        write: false,
        outdir: "out".into(),
        outbase: outbase.into(),
        format: BuildFormat::EsModule,
        ..BuildOptions::default()
    });
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    fixture.assert_paths(&result, &[output]);
    let callback_path = format!("./{filename}");
    let resolves = resolves.lock().unwrap().clone();
    let loads = loads.lock().unwrap().clone();
    assert_eq!(
        resolves,
        vec![json!({
            "path": callback_path,
            "namespace": "file",
            "importer": "",
            "resolveDir": fixture.0.to_string_lossy(),
            "kind": 1,
        })]
    );
    assert_eq!(loads, vec![format!("input {callback_path}")]);
    let expected = format!(
        "// virtual-ns:input {callback_path}\nconsole.log({});\n",
        serde_json::to_string(&format!("input {callback_path}")).unwrap()
    );
    assert_eq!(result.output_files[0].contents, expected.as_bytes());
    compare_go(
        &json!({
            "directory": fixture.0,
            "entries": [input],
            "advanced": [],
            "plugin": true,
            "outbase": outbase,
        }),
        &result,
        &resolves,
        &loads,
    );
}

#[test]
fn actual_leading_invalid_files_keep_implicit_prefix_after_virtual_rewrite() {
    for outbase in ["", "."] {
        assert_prefixed_file_rewrite("?leading.js", "?leading.js", outbase, "_leading.js");
        assert_prefixed_file_rewrite("|leading.js", "|leading.js", outbase, "_leading.js");
        assert_prefixed_file_rewrite("./?leading.js", "?leading.js", outbase, "_leading.js");
    }
}

#[test]
fn absent_leading_question_entry_keeps_original_virtual_name() {
    let fixture = Fixture::new(&[]);
    let result = fixture.run(&["?leading.js"], &[], true);
    fixture.assert_paths(&result, &["leading.js"]);
    assert_eq!(
        result.output_files[0].contents,
        b"// virtual-ns:input ?leading.js\nconsole.log(\"input ?leading.js\");\n"
    );
}

#[test]
fn star_globs_preserve_literal_question_marks_and_recursive_segments() {
    let fixture = Fixture::new(&[
        ("dir?/file?one.js", "console.log('literal one');"),
        ("dir?/nested/file?two.js", "console.log('literal two');"),
        (
            "dir?/fileXother.js",
            "throw 'question mark is not a wildcard';",
        ),
        (
            "dirX/file?other.js",
            "throw 'directory question mark is literal';",
        ),
    ]);
    let result = fixture.run(&["dir?/file?*.js"], &[], false);
    fixture.assert_paths(&result, &["dir_/file_one.js"]);
    let result = fixture.run(&["dir?/**/file?*.js"], &[], false);
    fixture.assert_paths(&result, &["dir_/file_one.js", "dir_/nested/file_two.js"]);
}
