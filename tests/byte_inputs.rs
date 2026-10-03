//! Raw stdin and plugin input bytes, including empty and invalid UTF-8 payloads.

use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildResult, BuildStdin, Loader, OnLoadOptions, OnLoadResult,
    OnResolveOptions, OnResolveResult, Plugin, WatchOptions, build, context,
};
use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "esbuild-team-byte-inputs-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn options(&self) -> BuildOptions {
        BuildOptions {
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            outdir: "out".into(),
            asset_names: "assets/[name]-[hash]".into(),
            metafile: true,
            ..BuildOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn go_loader(loader: Loader) -> u16 {
    match loader {
        Loader::Base64 => 1,
        Loader::Binary => 2,
        Loader::Copy => 3,
        Loader::DataUrl => 5,
        Loader::File => 8,
        _ => panic!("unexpected test loader"),
    }
}

fn run_go(request: &Value) -> Option<Value> {
    let executable = std::env::var_os("ESBUILD_RS_TEAM_GO_BYTE_INPUTS")?;
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
    if let Some(path) = std::env::var_os("ESBUILD_RS_TEAM_BYTE_INPUT_REPORT") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let mut line = serde_json::to_vec(&json!({"request": request, "go": go})).unwrap();
        line.push(b'\n');
        file.write_all(&line).unwrap();
    }
    Some(go)
}

fn compare_go(request: &Value, result: &BuildResult) {
    let Some(go) = run_go(request) else {
        return;
    };
    assert!(go["Errors"].is_null(), "{go}");
    assert!(go["Warnings"].is_null(), "{go}");
    if request["Kind"] == "plugin" {
        assert_eq!(go["Loads"], 1);
        assert_eq!(go["LaterLoads"], usize::from(request["Present"] != true));
    }
    let reference: BTreeMap<_, _> = go["Files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["Path"].as_str().unwrap().to_string(),
                (
                    STANDARD.decode(file["Contents"].as_str().unwrap()).unwrap(),
                    file["Hash"].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect();
    let actual: BTreeMap<_, _> = result
        .output_files
        .iter()
        .map(|file| {
            (
                file.path.clone(),
                (file.contents.clone(), file.hash.clone()),
            )
        })
        .collect();
    assert_eq!(actual, reference, "{request}");
    if request["Metafile"] == true {
        assert_eq!(
            serde_json::from_str::<Value>(&result.metafile).unwrap(),
            serde_json::from_str::<Value>(go["Metafile"].as_str().unwrap()).unwrap(),
            "{request}"
        );
    }
}

fn check_payload(result: &BuildResult, loader: Loader, payload: &[u8]) {
    const SCRIPT: &str = r"
const assert = require('node:assert/strict'), fs = require('node:fs'), path = require('node:path');
const req = JSON.parse(fs.readFileSync(0, 'utf8')), value = require(req.path);
let bytes;
if (req.loader === 1) bytes = Buffer.from(value, 'base64');
else if (req.loader === 2) { assert(value instanceof Uint8Array); bytes = value; }
else if (req.loader === 5) {
  const comma = value.indexOf(','), header = value.slice(0, comma), data = value.slice(comma + 1);
  assert(header.startsWith('data:'));
  bytes = header.endsWith(';base64') ? Buffer.from(data, 'base64') : Buffer.from(decodeURIComponent(data));
} else if (req.loader === 8) bytes = fs.readFileSync(path.resolve(path.dirname(req.path), value));
assert.deepEqual(Array.from(bytes), req.expected);
console.log('ok');
";
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    if loader == Loader::Copy {
        assert_eq!(result.output_files.len(), 1);
        assert_eq!(result.output_files[0].contents, payload);
        return;
    }
    for file in &result.output_files {
        let path = std::path::Path::new(&file.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, &file.contents).unwrap();
    }
    let code = &result
        .output_files
        .iter()
        .find(|file| {
            std::path::Path::new(&file.path)
                .extension()
                .is_some_and(|extension| extension == "js")
        })
        .unwrap()
        .path;
    let request = json!({"path": code, "loader": go_loader(loader), "expected": payload});
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
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok\n");
}

fn byte_plugin(
    bytes: Option<Vec<u8>>,
    text: Option<String>,
    loader: Loader,
    loads: Arc<AtomicUsize>,
    later: Arc<AtomicUsize>,
) -> Plugin {
    Plugin::new("raw-bytes", move |plugin_build| {
        plugin_build.on_resolve(
            OnResolveOptions {
                filter: "^virtual-input$".into(),
                ..OnResolveOptions::default()
            },
            |_| {
                Ok(OnResolveResult {
                    path: "payload.bin".into(),
                    namespace: "bytes".into(),
                    plugin_data: Some(Arc::new("resolve-data")),
                    ..OnResolveResult::default()
                })
            },
        );
        plugin_build.on_load(
            OnLoadOptions {
                filter: ".*".into(),
                namespace: "bytes".into(),
            },
            {
                let bytes = bytes.clone();
                let text = text.clone();
                let loads = loads.clone();
                move |args| {
                    assert_eq!(
                        args.plugin_data.as_ref().unwrap().downcast_ref::<&str>(),
                        Some(&"resolve-data")
                    );
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(OnLoadResult {
                        contents: text.clone(),
                        contents_bytes: bytes.clone(),
                        loader,
                        ..OnLoadResult::default()
                    })
                }
            },
        );
        plugin_build.on_load(
            OnLoadOptions {
                filter: ".*".into(),
                namespace: "bytes".into(),
            },
            {
                let later = later.clone();
                move |_| {
                    later.fetch_add(1, Ordering::SeqCst);
                    Ok(OnLoadResult {
                        contents_bytes: Some(vec![0xff, 0]),
                        loader: Loader::Base64,
                        ..OnLoadResult::default()
                    })
                }
            },
        );
        Ok(())
    })
}

#[test]
fn stdin_and_plugin_bytes_reach_all_raw_loaders_without_utf8_conversion() {
    let fixture = Fixture::new();
    for payload in [
        vec![0xff],
        vec![0xff, 0],
        vec![0xff, 0, 0xfe, b'A'],
        Vec::new(),
    ] {
        for loader in [
            Loader::Base64,
            Loader::Binary,
            Loader::DataUrl,
            Loader::File,
            Loader::Copy,
        ] {
            for kind in ["stdin", "plugin"] {
                let mut options = fixture.options();
                let loads = Arc::new(AtomicUsize::new(0));
                let later = Arc::new(AtomicUsize::new(0));
                if kind == "stdin" {
                    options.stdin = Some(BuildStdin {
                        contents: "WRONG TEXT".into(),
                        contents_bytes: Some(payload.clone()),
                        loader,
                        sourcefile: "payload.bin".into(),
                        ..BuildStdin::default()
                    });
                } else {
                    options.entry_points = vec!["virtual-input".into()];
                    options.plugins = vec![byte_plugin(
                        Some(payload.clone()),
                        Some("WRONG TEXT".into()),
                        loader,
                        loads.clone(),
                        later.clone(),
                    )];
                }
                let result = build(options);
                check_payload(&result, loader, &payload);
                assert_eq!(loads.load(Ordering::SeqCst), usize::from(kind == "plugin"));
                assert_eq!(later.load(Ordering::SeqCst), 0);
                compare_go(
                    &json!({"Kind": kind, "UseBytes": true, "Payload": STANDARD.encode(&payload), "Present": true, "Loader": go_loader(loader), "Dir": fixture.0, "Outdir": "out", "Sourcefile": "payload.bin", "Metafile": true}),
                    &result,
                );
            }
        }
    }
}

#[test]
fn text_inputs_remain_utf8_and_original_base64_output_assertions_hold_natively() {
    let fixture = Fixture::new();
    for (text, bytes, expected) in [
        ("ÿ", None, "w78="),
        ("WRONG TEXT", Some(vec![0xff]), "/w=="),
        ("WRONG TEXT", Some(Vec::new()), ""),
    ] {
        let result = build(BuildOptions {
            abs_working_dir: fixture.0.to_string_lossy().into_owned(),
            stdin: Some(BuildStdin {
                contents: text.into(),
                contents_bytes: bytes.clone(),
                loader: Loader::Base64,
                ..BuildStdin::default()
            }),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty() && result.warnings.is_empty());
        assert_eq!(result.output_files.len(), 1);
        assert_eq!(
            result.output_files[0].contents,
            format!("module.exports = \"{expected}\";\n").as_bytes()
        );
        compare_go(
            &json!({"Kind": "stdin", "UseBytes": bytes.is_some(), "Payload": STANDARD.encode(bytes.as_deref().unwrap_or_default()), "Text": text, "Loader": 1, "Dir": fixture.0}),
            &result,
        );
    }
    let options = BuildOptions {
        entry_points: vec!["virtual-input".into()],
        plugins: vec![byte_plugin(
            None,
            Some("ÿ".into()),
            Loader::Base64,
            Arc::default(),
            Arc::default(),
        )],
        ..fixture.options()
    };
    let result = build(options);
    check_payload(&result, Loader::Base64, "ÿ".as_bytes());
    compare_go(
        &json!({"Kind": "plugin", "Text": "ÿ", "Present": true, "Loader": 1, "Dir": fixture.0, "Outdir": "out", "Sourcefile": "payload.bin", "Metafile": true}),
        &result,
    );
}

#[test]
fn plugin_empty_bytes_claim_the_module_and_absent_contents_defer() {
    let fixture = Fixture::new();
    for (bytes, text) in [
        (None, None),
        (Some(Vec::new()), Some("WRONG TEXT".into())),
        (None, Some(String::new())),
    ] {
        let claimed = bytes.is_some() || text.is_some();
        let loads = Arc::new(AtomicUsize::new(0));
        let later = Arc::new(AtomicUsize::new(0));
        let result = build(BuildOptions {
            entry_points: vec!["virtual-input".into()],
            plugins: vec![byte_plugin(
                bytes.clone(),
                text.clone(),
                Loader::Base64,
                loads.clone(),
                later.clone(),
            )],
            ..fixture.options()
        });
        check_payload(
            &result,
            Loader::Base64,
            if claimed { &[] } else { &[0xff, 0] },
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(later.load(Ordering::SeqCst), usize::from(!claimed));
        let request = json!({"Kind": "plugin", "UseBytes": bytes.is_some(), "Payload": "", "Text": text.unwrap_or_default(), "Present": claimed, "Loader": 1, "Dir": fixture.0, "Outdir": "out", "Sourcefile": "payload.bin", "Metafile": true});
        compare_go(&request, &result);
    }
}

#[test]
fn byte_js_modules_forward_plugin_data_to_dependency_resolution() {
    let fixture = Fixture::new();
    let checks = Arc::new(AtomicUsize::new(0));
    let plugin = Plugin::new("raw-bytes", {
        let checks = checks.clone();
        move |plugin_build| {
            plugin_build.on_resolve(
                OnResolveOptions {
                    filter: "^(virtual-input|payload)$".into(),
                    ..OnResolveOptions::default()
                },
                {
                    let checks = checks.clone();
                    move |args| {
                        if args.path == "payload" {
                            assert_eq!(
                                args.plugin_data
                                    .as_ref()
                                    .unwrap()
                                    .downcast_ref::<Vec<u8>>()
                                    .unwrap(),
                                &[0xff, 0]
                            );
                            checks.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(OnResolveResult {
                            path: if args.path == "payload" {
                                "payload.bin"
                            } else {
                                "entry.js"
                            }
                            .into(),
                            namespace: "bytes".into(),
                            plugin_data: Some(Arc::new(args.path)),
                            ..OnResolveResult::default()
                        })
                    }
                },
            );
            plugin_build.on_load(
                OnLoadOptions {
                    filter: ".*".into(),
                    namespace: "bytes".into(),
                },
                {
                    let checks = checks.clone();
                    move |args| {
                        let entry = args.path == "entry.js";
                        assert_eq!(
                            args.plugin_data
                                .as_ref()
                                .unwrap()
                                .downcast_ref::<String>()
                                .unwrap(),
                            if entry { "virtual-input" } else { "payload" }
                        );
                        checks.fetch_add(1, Ordering::SeqCst);
                        Ok(OnLoadResult {
                            contents: Some("INVALID TEXT THAT MUST NOT BE PARSED".into()),
                            contents_bytes: Some(if entry {
                                b"import value from 'payload'; console.log(value);".to_vec()
                            } else {
                                vec![0xff, 0]
                            }),
                            plugin_data: entry
                                .then(|| Arc::new(vec![0xff_u8, 0]) as esbuild_rs::api::PluginData),
                            loader: if entry { Loader::Js } else { Loader::Base64 },
                            ..OnLoadResult::default()
                        })
                    }
                },
            );
            Ok(())
        }
    });
    let result = build(BuildOptions {
        bundle: true,
        format: BuildFormat::CommonJs,
        entry_points: vec!["virtual-input".into()],
        plugins: vec![plugin],
        ..fixture.options()
    });
    assert!(result.errors.is_empty() && result.warnings.is_empty());
    assert_eq!(checks.load(Ordering::SeqCst), 3);
    let path = fixture.0.join("graph.cjs");
    std::fs::write(&path, &result.output_files[0].contents).unwrap();
    let output = Command::new("node").arg(&path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"/wA=\n");
    compare_go(
        &json!({"Kind": "graph", "UseBytes": true, "Payload": "/wA=", "Present": true, "Loader": 1, "Dir": fixture.0, "Outdir": "out", "Sourcefile": "entry.js", "Metafile": true}),
        &result,
    );
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "plugin watch lifecycle timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn watch_plugin(
    watched: PathBuf,
    directory: PathBuf,
    setup: Arc<AtomicUsize>,
    ended: Arc<AtomicUsize>,
    disposed: Arc<AtomicUsize>,
) -> Plugin {
    Plugin::new("raw-bytes", move |plugin_build| {
        setup.fetch_add(1, Ordering::SeqCst);
        plugin_build.on_resolve(
            OnResolveOptions {
                filter: "^virtual-input$".into(),
                ..OnResolveOptions::default()
            },
            |_| {
                Ok(OnResolveResult {
                    path: "payload.bin".into(),
                    namespace: "bytes".into(),
                    ..OnResolveResult::default()
                })
            },
        );
        plugin_build.on_load(
            OnLoadOptions {
                filter: ".*".into(),
                namespace: "bytes".into(),
            },
            {
                let watched = watched.clone();
                let directory = directory.clone();
                move |_| {
                    let mut bytes = std::fs::read(&watched).unwrap();
                    bytes.push(
                        u8::try_from(std::fs::read_dir(&directory).unwrap().count()).unwrap(),
                    );
                    Ok(OnLoadResult {
                        contents_bytes: Some(bytes),
                        loader: Loader::Base64,
                        watch_files: vec![watched.to_string_lossy().into_owned()],
                        watch_dirs: vec![directory.to_string_lossy().into_owned()],
                        ..OnLoadResult::default()
                    })
                }
            },
        );
        plugin_build.on_end({
            let ended = ended.clone();
            move |result| {
                assert!(result.errors.is_empty());
                ended.fetch_add(1, Ordering::SeqCst);
                Ok(esbuild_rs::api::OnEndResult::default())
            }
        });
        plugin_build.on_dispose({
            let disposed = disposed.clone();
            move || {
                disposed.fetch_add(1, Ordering::SeqCst);
            }
        });
        Ok(())
    })
}

#[test]
fn byte_plugin_watch_files_dirs_and_disposal_survive_rebuilds() {
    let fixture = Fixture::new();
    let watched = fixture.0.join("watch.bin");
    let directory = fixture.0.join("watched");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&watched, [0xff, 0]).unwrap();
    let setup = Arc::new(AtomicUsize::new(0));
    let ended = Arc::new(AtomicUsize::new(0));
    let disposed = Arc::new(AtomicUsize::new(0));
    let plugin = watch_plugin(
        watched.clone(),
        directory.clone(),
        setup.clone(),
        ended.clone(),
        disposed.clone(),
    );
    let build_context = context(BuildOptions {
        entry_points: vec!["virtual-input".into()],
        outfile: "out.js".into(),
        write: true,
        plugins: vec![plugin],
        abs_working_dir: fixture.0.to_string_lossy().into_owned(),
        ..BuildOptions::default()
    })
    .unwrap();
    build_context.watch(WatchOptions::default()).unwrap();
    let output = fixture.0.join("out.js");
    let mut snapshots = Vec::new();
    for (step, expected) in ["/wAA", "AP8A", "AP8B"].iter().enumerate() {
        if step == 1 {
            std::fs::write(&watched, [0, 0xff]).unwrap();
        }
        if step == 2 {
            std::fs::write(directory.join("new-entry"), []).unwrap();
        }
        wait_until(|| {
            std::fs::read_to_string(&output).is_ok_and(|code| code.contains(expected))
                && ended.load(Ordering::SeqCst) > step
        });
        snapshots.push(std::fs::read_to_string(&output).unwrap());
    }
    assert_eq!(setup.load(Ordering::SeqCst), 1);
    assert!(ended.load(Ordering::SeqCst) >= 3);
    build_context.dispose();
    build_context.dispose();
    wait_until(|| disposed.load(Ordering::SeqCst) == 1);
    assert!(build_context.rebuild().output_files.is_empty());
    let go_directory = fixture.0.join("go-watch");
    std::fs::create_dir(&go_directory).unwrap();
    if let Some(go) = run_go(&json!({"Kind": "watch", "Dir": go_directory})) {
        assert_eq!(go["Snapshots"], json!(snapshots));
        assert_eq!(go["Setup"], 1);
        assert_eq!(go["Dispose"], 1);
        assert!(go["End"].as_u64().unwrap() >= 3);
    }
}
