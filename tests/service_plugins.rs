//! Callback wire invariants supplementary to unchanged upstream plugin tests.
//! Set `ESBUILD_RS_SERVICE_PLUGIN_BINARY` to the pinned Go executable to run the
//! `plugin_transport_*` cases against the reference protocol implementation.

use std::{
    fs,
    io::{self, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use esbuild_rs::service::{Packet, VERSION, Value, decode_packet, encode_packet};

struct Host {
    child: Child,
    input: Option<ChildStdin>,
    frames: mpsc::Receiver<io::Result<Vec<u8>>>,
}

impl Host {
    fn start(ping: bool) -> Self {
        let mut command = Command::new(
            std::env::var_os("ESBUILD_RS_SERVICE_PLUGIN_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_esbuild").into()),
        );
        command.arg(format!("--service={VERSION}"));
        if ping {
            command.arg("--ping");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let mut output = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let mut length = [0; 4];
                if output.read_exact(&mut length).is_err() {
                    break;
                }
                let mut bytes = vec![0; usize::try_from(u32::from_le_bytes(length)).unwrap()];
                match output.read_exact(&mut bytes) {
                    Ok(()) => {
                        if sender.send(Ok(bytes)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        let host = Self {
            child,
            input,
            frames,
        };
        assert_eq!(host.frame(), VERSION.as_bytes());
        host
    }

    fn frame(&self) -> Vec<u8> {
        self.frames
            .recv_timeout(Duration::from_secs(5))
            .expect("service must flush each frame promptly")
            .unwrap()
    }

    fn packet(&self) -> Packet {
        decode_packet(&self.frame()).unwrap()
    }

    fn send(&mut self, packet: &Packet) {
        let bytes = encode_packet(packet).unwrap();
        // Every byte is its own write to exercise arbitrary input fragmentation.
        for byte in bytes {
            self.input.as_mut().unwrap().write_all(&[byte]).unwrap();
        }
    }

    fn request(&mut self, id: u32, value: Value) {
        self.send(&Packet {
            id,
            is_request: true,
            value,
        });
    }

    fn eof(&mut self) {
        self.input.take();
    }

    fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "service exit: {status}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "service must exit after draining EOF"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn callback(id: i64, filter: &str, namespace: &str) -> Value {
    Value::object([
        ("id", Value::Int(id)),
        ("filter", Value::from(filter)),
        ("namespace", Value::from(namespace)),
    ])
}

fn plugin(name: &str, resolves: Vec<Value>, loads: Vec<Value>) -> Value {
    Value::object([
        ("name", Value::from(name)),
        ("onStart", Value::Bool(false)),
        ("onEnd", Value::Bool(true)),
        ("onResolve", Value::Array(resolves)),
        ("onLoad", Value::Array(loads)),
    ])
}

fn build_request(key: i64, entry: &str, plugins: Vec<Value>) -> Value {
    Value::object([
        ("command", Value::from("build")),
        ("context", Value::Bool(false)),
        ("key", Value::Int(key)),
        ("write", Value::Bool(false)),
        ("absWorkingDir", Value::from("")),
        ("nodePaths", Value::Array(vec![])),
        (
            "entries",
            Value::Array(vec![Value::Array(vec![
                Value::from(""),
                Value::from(entry),
            ])]),
        ),
        (
            "flags",
            Value::Array(vec![
                Value::from("--bundle"),
                Value::from("--format=cjs"),
                Value::from("--log-level=silent"),
            ]),
        ),
        ("plugins", Value::Array(plugins)),
    ])
}

fn diagnostics() -> Value {
    Value::object([
        ("errors", Value::Array(vec![])),
        ("warnings", Value::Array(vec![])),
    ])
}

fn reply(host: &mut Host, id: u32, value: Value) {
    host.send(&Packet {
        id,
        is_request: false,
        value,
    });
}

fn expect_callback(host: &Host, command: &str, key: i64) -> Packet {
    let packet = host.packet();
    assert!(packet.is_request);
    assert_eq!(
        packet.value.get("command").and_then(Value::as_str),
        Some(command)
    );
    assert_eq!(packet.value.get("key").and_then(Value::as_int), Some(key));
    packet
}

fn assert_success(packet: &Packet, id: u32) {
    assert!(!packet.is_request);
    assert_eq!(packet.id, id);
    assert_eq!(
        packet.value.get("errors").and_then(Value::as_array),
        Some([].as_slice())
    );
}

fn output_text(packet: &Packet) -> String {
    let files = packet
        .value
        .get("outputFiles")
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(files.len(), 1);
    String::from_utf8(
        files[0]
            .get("contents")
            .and_then(Value::as_bytes)
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn resolve_request(key: i64, path: &str) -> Value {
    Value::object([
        ("command", Value::from("resolve")),
        ("key", Value::Int(key)),
        ("path", Value::from(path)),
        ("kind", Value::from("entry-point")),
        ("pluginName", Value::from("caller")),
    ])
}

fn set(value: &mut Value, key: &str, item: Value) {
    let Value::Object(object) = value else {
        panic!("expected object")
    };
    object.insert(key.as_bytes().to_vec(), item);
}

#[test]
fn plugin_transport_ordered_matching_ids_and_opaque_data_round_trip() {
    let mut host = Host::start(false);
    host.request(
        0,
        build_request(
            91,
            "foo",
            vec![
                plugin("first", vec![callback(11, "^foo$", "")], vec![]),
                plugin(
                    "excluded",
                    vec![callback(12, "^foo$", "other")],
                    vec![callback(32, ".*", "other")],
                ),
                plugin(
                    "third",
                    vec![callback(13, "(?i)^FOO$", "")],
                    vec![callback(31, "^foo$", "virtual")],
                ),
            ],
        ),
    );
    let start = expect_callback(&host, "on-start", 91);
    // Packet ID zero is also a valid outgoing callback ID.
    assert_eq!(start.id, 0);
    reply(&mut host, start.id, diagnostics());
    let resolve = expect_callback(&host, "on-resolve", 91);
    assert_eq!(
        resolve.value.get("namespace").and_then(Value::as_str),
        Some("")
    );
    assert_eq!(
        resolve.value.get("ids"),
        Some(&Value::Array(vec![Value::Int(11), Value::Int(13)]))
    );
    assert_eq!(resolve.value.get("pluginData"), Some(&Value::Null));
    reply(
        &mut host,
        resolve.id,
        Value::object([
            ("id", Value::Int(13)),
            ("path", Value::from("foo")),
            ("namespace", Value::from("virtual")),
            ("suffix", Value::from("?query")),
            ("pluginData", Value::Int(42)),
        ]),
    );
    let load = expect_callback(&host, "on-load", 91);
    assert_eq!(
        load.value.get("ids"),
        Some(&Value::Array(vec![Value::Int(31)]))
    );
    assert_eq!(load.value.get("pluginData"), Some(&Value::Int(42)));
    assert_eq!(
        load.value.get("suffix").and_then(Value::as_str),
        Some("?query")
    );
    reply(
        &mut host,
        load.id,
        Value::object([
            ("id", Value::Int(31)),
            ("contents", Value::Bytes(vec![0, 255])),
            ("loader", Value::from("base64")),
            ("pluginData", Value::Int(43)),
        ]),
    );
    let result = host.packet();
    assert_success(&result, 0);
    assert!(output_text(&result).contains("\"AP8=\""));
    // Standalone onEnd/onDispose have no second service callback.
    host.eof();
    host.wait();
}

#[test]
fn plugin_transport_nested_resolve_keeps_reader_live_and_retires_build_key() {
    let mut host = Host::start(false);
    host.request(
        17,
        build_request(
            901,
            "entry",
            vec![plugin(
                "nested",
                vec![callback(11, "^(entry|nested)$", "")],
                vec![callback(21, ".*", "virtual")],
            )],
        ),
    );
    let start = expect_callback(&host, "on-start", 901);
    reply(&mut host, start.id, diagnostics());
    let first = expect_callback(&host, "on-resolve", 901);
    let mut nested = resolve_request(901, "nested");
    set(&mut nested, "pluginData", Value::Int(42));
    set(
        &mut nested,
        "with",
        Value::object([("type", Value::from("json"))]),
    );
    host.request(18, nested);
    let nested = expect_callback(&host, "on-resolve", 901);
    assert_eq!(
        nested.value.get("path").and_then(Value::as_str),
        Some("nested")
    );
    assert_eq!(nested.value.get("pluginData"), Some(&Value::Int(42)));
    assert_eq!(
        nested.value.get("with"),
        Some(&Value::object([("type", Value::from("json"))]))
    );
    reply(
        &mut host,
        nested.id,
        Value::object([
            ("id", Value::Int(11)),
            ("path", Value::from("nested")),
            ("namespace", Value::from("virtual")),
            ("sideEffects", Value::Bool(false)),
            ("pluginData", Value::Int(43)),
            ("suffix", Value::from("#suffix")),
        ]),
    );
    let resolved = host.packet();
    assert_success(&resolved, 18);
    assert_eq!(resolved.value.get("pluginData"), Some(&Value::Int(43)));
    assert_eq!(resolved.value.get("sideEffects"), Some(&Value::Bool(false)));
    assert_eq!(
        resolved.value.get("namespace").and_then(Value::as_str),
        Some("virtual")
    );
    assert_eq!(
        resolved.value.get("suffix").and_then(Value::as_str),
        Some("#suffix")
    );
    reply(
        &mut host,
        first.id,
        Value::object([
            ("id", Value::Int(11)),
            ("path", Value::from("entry")),
            ("namespace", Value::from("virtual")),
        ]),
    );
    let load = expect_callback(&host, "on-load", 901);
    reply(
        &mut host,
        load.id,
        Value::object([
            ("id", Value::Int(21)),
            ("contents", Value::Bytes(b"console.log(1)".to_vec())),
        ]),
    );
    assert_success(&host.packet(), 17);
    host.request(19, resolve_request(901, "nested"));
    let inactive = host.packet();
    assert_eq!(inactive.id, 19);
    assert_eq!(
        inactive.value.get("error").and_then(Value::as_str),
        Some("Cannot call \"resolve\" on an inactive build")
    );
    host.eof();
    host.wait();
}

#[test]
fn plugin_transport_empty_and_absent_load_contents_keep_disk_fallback_distinct() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "esbuild-service-plugin-fallback-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).unwrap();
    let file = root.join("input.js");
    fs::write(&file, "console.log(123)").unwrap();
    let file = fs::canonicalize(file).unwrap();
    for present in [false, true] {
        let mut host = Host::start(false);
        host.request(
            7,
            build_request(
                5,
                file.to_str().unwrap(),
                vec![plugin("load", vec![], vec![callback(21, "\\.js$", "file")])],
            ),
        );
        let start = expect_callback(&host, "on-start", 5);
        reply(&mut host, start.id, diagnostics());
        let load = expect_callback(&host, "on-load", 5);
        let mut response = Value::object([("id", Value::Int(21))]);
        if present {
            set(&mut response, "contents", Value::Bytes(vec![]));
        }
        reply(&mut host, load.id, response);
        let result = host.packet();
        assert_success(&result, 7);
        assert_eq!(output_text(&result).contains("console.log(123)"), !present);
        host.eof();
        host.wait();
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn plugin_transport_concurrent_builds_have_independent_keys_and_callback_ids() {
    let mut host = Host::start(false);
    for (id, key) in [(0, 400), (1, 401)] {
        host.request(
            id,
            build_request(
                key,
                "entry",
                vec![plugin(
                    "shared-name",
                    vec![callback(11, ".*", "")],
                    vec![callback(21, ".*", "virtual")],
                )],
            ),
        );
    }
    let mut completed = std::collections::BTreeMap::new();
    while completed.len() < 2 {
        let packet = host.packet();
        if !packet.is_request {
            assert_success(&packet, packet.id);
            assert!(completed.insert(packet.id, output_text(&packet)).is_none());
            continue;
        }
        let key = packet.value.get("key").and_then(Value::as_int).unwrap();
        assert!([400, 401].contains(&key));
        let response = match packet.value.get("command").and_then(Value::as_str).unwrap() {
            "on-start" => diagnostics(),
            "on-resolve" => Value::object([
                ("id", Value::Int(11)),
                ("path", Value::from("entry")),
                ("namespace", Value::from("virtual")),
                ("pluginData", Value::Int(key)),
            ]),
            "on-load" => {
                assert_eq!(packet.value.get("pluginData"), Some(&Value::Int(key)));
                Value::object([
                    ("id", Value::Int(21)),
                    (
                        "contents",
                        Value::Bytes(format!("console.log({key})").into_bytes()),
                    ),
                ])
            }
            command => panic!("unexpected callback: {command}"),
        };
        reply(&mut host, packet.id, response);
    }
    assert!(completed[&0].contains("console.log(400)"));
    assert!(completed[&1].contains("console.log(401)"));
    host.eof();
    host.wait();
}

fn message(detail: i64, text: &str) -> Value {
    Value::object([
        ("id", Value::from("")),
        ("pluginName", Value::from("original-plugin")),
        ("text", Value::from(text)),
        ("location", Value::Null),
        ("notes", Value::Array(vec![])),
        ("detail", Value::Int(detail)),
    ])
}

#[test]
fn plugin_transport_diagnostic_details_round_trip_with_plugin_name() {
    let mut host = Host::start(false);
    host.request(
        8,
        build_request(
            81,
            "entry",
            vec![plugin(
                "original-plugin",
                vec![callback(11, ".*", "")],
                vec![],
            )],
        ),
    );
    let start = expect_callback(&host, "on-start", 81);
    reply(&mut host, start.id, diagnostics());
    let resolve = expect_callback(&host, "on-resolve", 81);
    reply(
        &mut host,
        resolve.id,
        Value::object([
            ("id", Value::Int(11)),
            ("errors", Value::Array(vec![message(71, "returned error")])),
            (
                "warnings",
                Value::Array(vec![message(72, "returned warning")]),
            ),
        ]),
    );
    let result = host.packet();
    assert!(!result.is_request);
    assert_eq!(result.id, 8);
    for (key, detail, text) in [
        ("errors", 71, "returned error"),
        ("warnings", 72, "returned warning"),
    ] {
        let messages = result.value.get(key).and_then(Value::as_array).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("detail"), Some(&Value::Int(detail)));
        assert_eq!(messages[0].get("text").and_then(Value::as_str), Some(text));
        assert_eq!(
            messages[0].get("pluginName").and_then(Value::as_str),
            Some("original-plugin")
        );
    }
    host.eof();
    host.wait();
}

#[test]
fn native_plugin_invalid_filters_do_not_leave_active_resolve_registration() {
    let mut host = Host::start(false);
    host.request(
        3,
        build_request(
            44,
            "entry",
            vec![plugin(
                "bad-filter",
                vec![callback(11, "x(?=y)", "")],
                vec![],
            )],
        ),
    );
    let error = host.packet();
    assert!(!error.is_request);
    assert_eq!(error.id, 3);
    assert_eq!(
        error.value.get("error").and_then(Value::as_str),
        Some("[bad-filter] \"onResolve\" filter is not a valid Go regular expression: \"x(?=y)\"")
    );
    host.request(4, resolve_request(44, "entry"));
    let inactive = host.packet();
    assert_eq!(
        inactive.value.get("error").and_then(Value::as_str),
        Some("Cannot call \"resolve\" on an inactive build")
    );
    host.eof();
    host.wait();
}

#[test]
fn native_plugin_disconnected_host_wakes_blocked_callback_and_drains_error() {
    let mut host = Host::start(false);
    host.request(
        9,
        build_request(
            11,
            "entry",
            vec![plugin("disconnect", vec![callback(11, ".*", "")], vec![])],
        ),
    );
    expect_callback(&host, "on-start", 11);
    host.eof();
    let result = host.packet();
    assert!(!result.is_request);
    assert_eq!(result.id, 9);
    let errors = result
        .value
        .get("errors")
        .and_then(Value::as_array)
        .unwrap();
    assert!(
        errors
            .iter()
            .any(|error| error.get("text").and_then(Value::as_str)
                == Some("The service was stopped"))
    );
    host.wait();
}

#[test]
fn plugin_transport_invalid_loader_and_filter_errors_use_pinned_go_quoting() {
    for (loader, quoted, name_override) in [
        ("unknown", "\"unknown\"", None),
        ("\u{1c89}", "\"\\u1c89\"", None),
        ("\0", "\"\\x00\"", None),
        ("unknown", "\"unknown\"", Some("custom-name")),
    ] {
        let mut host = Host::start(false);
        host.request(
            6,
            build_request(
                65,
                "entry",
                vec![plugin(
                    "loader",
                    vec![callback(11, ".*", "")],
                    vec![callback(21, ".*", "virtual")],
                )],
            ),
        );
        let start = expect_callback(&host, "on-start", 65);
        reply(&mut host, start.id, diagnostics());
        let resolve = expect_callback(&host, "on-resolve", 65);
        reply(
            &mut host,
            resolve.id,
            Value::object([
                ("id", Value::Int(11)),
                ("path", Value::from("entry")),
                ("namespace", Value::from("virtual")),
            ]),
        );
        let load = expect_callback(&host, "on-load", 65);
        let mut response = Value::object([
            ("id", Value::Int(21)),
            ("loader", Value::from(loader)),
            ("contents", Value::Bytes(vec![])),
        ]);
        if let Some(name) = name_override {
            set(&mut response, "pluginName", Value::from(name));
        }
        reply(&mut host, load.id, response);
        let result = host.packet();
        assert!(!result.is_request);
        let errors = result
            .value
            .get("errors")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].get("pluginName").and_then(Value::as_str),
            Some(name_override.unwrap_or("loader"))
        );
        assert_eq!(
            errors[0].get("text").and_then(Value::as_str),
            Some(format!("Invalid loader value: {quoted}").as_str())
        );
        host.eof();
        host.wait();
    }
    // The Go service retains a failed setup build until process shutdown. This
    // compares its error packet, without asserting native cleanup behavior.
    for kind in ["onResolve", "onLoad"] {
        let mut host = Host::start(false);
        let filter = callback(11, "x(?=y)\u{1c89}", "");
        let (resolves, loads) = if kind == "onResolve" {
            (vec![filter], vec![])
        } else {
            (vec![], vec![filter])
        };
        host.request(
            3,
            build_request(44, "entry", vec![plugin("quoted", resolves, loads)]),
        );
        let result = host.packet();
        assert_eq!(result.value.get("error").and_then(Value::as_str), Some(format!("[quoted] \"{kind}\" filter is not a valid Go regular expression: \"x(?=y)\\u1c89\"").as_str()));
    }
}

#[test]
fn native_plugin_unknown_watch_and_unsupported_serve_return_errors() {
    let mut host = Host::start(false);
    let mut request = build_request(100, "entry", vec![]);
    set(&mut request, "context", Value::Bool(true));
    host.request(0, request);
    let response = host.packet();
    assert_eq!(response.id, 0);
    assert!(
        response
            .value
            .get("errors")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    for (id, command, key) in [(2, "watch", 999), (4, "serve", 100)] {
        host.request(
            id,
            Value::object([("command", Value::from(command)), ("key", Value::Int(key))]),
        );
        let response = host.packet();
        assert_eq!(response.id, id);
        let error = response.value.get("error").and_then(Value::as_str).unwrap();
        if command == "watch" {
            assert_eq!(error, "Cannot watch");
        } else {
            assert!(error.contains("not implemented"));
        }
    }
    host.request(
        5,
        Value::object([
            ("command", Value::from("dispose")),
            ("key", Value::Int(100)),
        ]),
    );
    assert!(host.packet().value.as_object().unwrap().is_empty());
    host.eof();
    host.wait();
}

#[test]
fn plugin_transport_callback_error_preserves_selected_plugin_name() {
    for command in ["on-resolve", "on-load"] {
        let mut host = Host::start(false);
        host.request(
            6,
            build_request(
                65,
                "entry",
                vec![plugin(
                    "selected",
                    vec![callback(11, ".*", "")],
                    vec![callback(21, ".*", "virtual")],
                )],
            ),
        );
        let start = expect_callback(&host, "on-start", 65);
        reply(&mut host, start.id, diagnostics());
        let resolve = expect_callback(&host, "on-resolve", 65);
        let (callback_id, response_id) = if command == "on-load" {
            reply(
                &mut host,
                resolve.id,
                Value::object([
                    ("id", Value::Int(11)),
                    ("path", Value::from("entry")),
                    ("namespace", Value::from("virtual")),
                ]),
            );
            (expect_callback(&host, "on-load", 65).id, 21)
        } else {
            (resolve.id, 11)
        };
        reply(
            &mut host,
            callback_id,
            Value::object([
                ("id", Value::Int(response_id)),
                ("pluginName", Value::from("ignored-override")),
                ("error", Value::from("callback failure")),
            ]),
        );
        let result = host.packet();
        let errors = result
            .value
            .get("errors")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].get("text").and_then(Value::as_str),
            Some("callback failure")
        );
        // Upstream handles error before the optional pluginName override.
        assert_eq!(
            errors[0].get("pluginName").and_then(Value::as_str),
            Some("selected")
        );
        host.eof();
        host.wait();
    }
}

#[test]
fn native_plugin_on_end_hook_decodes_acknowledged_diagnostics_once() {
    use esbuild_rs::{
        api,
        service::plugins::{PluginBridge, ResolveCallbacks, SendRequest},
    };
    use std::sync::Arc;
    let ended = Arc::new(AtomicUsize::new(0));
    let send_request: SendRequest = {
        let ended = ended.clone();
        Arc::new(move |request| {
            assert_eq!(request.get("key"), Some(&Value::Int(23)));
            match request.get("command").and_then(Value::as_str).unwrap() {
                "on-start" => Ok(diagnostics()),
                "on-end" => {
                    ended.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        request.get("errors").and_then(Value::as_array),
                        Some([].as_slice())
                    );
                    Ok(Value::object([
                        ("errors", Value::Array(vec![message(79, "onEnd failure")])),
                        ("warnings", Value::Array(vec![])),
                    ]))
                }
                command => panic!("unexpected callback: {command}"),
            }
        })
    };
    let registry = ResolveCallbacks::default();
    let bridge = PluginBridge::new(
        23,
        &Value::Array(vec![plugin("end", vec![], vec![])]),
        send_request,
        registry.clone(),
    )
    .unwrap();
    assert!(bridge.has_on_end());
    let result = api::build(api::BuildOptions {
        stdin: Some(api::BuildStdin {
            contents: "let x=1".into(),
            ..api::BuildStdin::default()
        }),
        plugins: vec![
            bridge.plugin(),
            bridge.on_end_plugin(|result| {
                Value::object([
                    (
                        "errors",
                        Value::Array(
                            result
                                .errors
                                .iter()
                                .map(|error| message(-1, &error.text))
                                .collect(),
                        ),
                    ),
                    ("warnings", Value::Array(vec![])),
                ])
            }),
        ],
        ..api::BuildOptions::default()
    });
    assert_eq!(ended.load(Ordering::SeqCst), 1);
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].text, "onEnd failure");
    drop(bridge);
    assert!(registry.lock().unwrap().is_empty());
}
