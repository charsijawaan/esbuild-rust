//! Deterministic lifecycle/transport checks in addition to unchanged original
//! context registrations. `ESBUILD_RS_SERVICE_CONTEXT_BINARY` can select Go.

use esbuild_rs::service::{Packet, VERSION, Value, decode_packet, encode_packet};
use std::{
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Host {
    child: Child,
    input: Option<ChildStdin>,
    frames: mpsc::Receiver<Vec<u8>>,
}

impl Host {
    fn start() -> Self {
        Self::start_with_ping(false)
    }
    fn start_with_ping(ping: bool) -> Self {
        let mut command = Command::new(
            std::env::var_os("ESBUILD_RS_SERVICE_CONTEXT_BINARY")
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
        let (send, frames) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let mut length = [0; 4];
                if output.read_exact(&mut length).is_err() {
                    break;
                }
                let mut bytes = vec![0; u32::from_le_bytes(length) as usize];
                if output.read_exact(&mut bytes).is_err() || send.send(bytes).is_err() {
                    break;
                }
            }
        });
        let host = Self {
            child,
            input,
            frames,
        };
        assert_eq!(
            host.frames.recv_timeout(Duration::from_secs(5)).unwrap(),
            VERSION.as_bytes()
        );
        host
    }

    fn send(&mut self, packet: &Packet) {
        let bytes = encode_packet(packet).unwrap();
        // Fragment both callback replies and requests across arbitrary bytes.
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
    fn reply(&mut self, id: u32, value: Value) {
        self.send(&Packet {
            id,
            is_request: false,
            value,
        });
    }
    fn packet(&self) -> Packet {
        decode_packet(
            &self
                .frames
                .recv_timeout(Duration::from_secs(5))
                .expect("context service must not deadlock"),
        )
        .unwrap()
    }
    fn quiet(&self) {
        assert!(
            matches!(
                self.frames.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "operation must still wait for the outstanding callback"
        );
    }
    fn response(&self, id: u32) -> Value {
        let packet = self.packet();
        assert!(!packet.is_request);
        assert_eq!(packet.id, id);
        packet.value
    }
    fn callback(&self, command: &str) -> Packet {
        let packet = self.packet();
        assert!(packet.is_request);
        assert_eq!(
            packet.value.get("command").and_then(Value::as_str),
            Some(command)
        );
        packet
    }
    fn finish(&mut self) {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "context service exit: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "context service EOF must drain");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn dispose(&mut self, key: i64, id: u32) {
        self.request(id, operation("dispose", key));
        assert!(self.response(id).as_object().unwrap().is_empty());
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn set(value: &mut Value, key: &str, item: Value) {
    let Value::Object(object) = value else {
        panic!("expected object");
    };
    object.insert(key.as_bytes().to_vec(), item);
}
fn flags(items: &[&str]) -> Value {
    Value::Array(items.iter().map(|item| Value::from(*item)).collect())
}
fn context(key: i64) -> Value {
    Value::object([
        ("command", Value::from("build")),
        ("context", Value::Bool(true)),
        ("key", Value::Int(key)),
        ("write", Value::Bool(false)),
        ("entries", Value::Array(vec![])),
        ("nodePaths", Value::Array(vec![])),
        ("absWorkingDir", Value::from("")),
        ("flags", flags(&["--log-level=silent", "--format=esm"])),
        (
            "stdinContents",
            Value::Bytes(b"export const value = 1".to_vec()),
        ),
    ])
}
fn operation(command: &str, key: i64) -> Value {
    Value::object([("command", Value::from(command)), ("key", Value::Int(key))])
}
fn diagnostics() -> Value {
    Value::object([
        ("errors", Value::Array(vec![])),
        ("warnings", Value::Array(vec![])),
    ])
}
fn assert_ok(response: &Value) {
    assert!(response.get("error").is_none(), "{response:?}");
    assert!(
        response
            .get("errors")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty(),
        "{response:?}"
    );
    assert!(
        response
            .get("warnings")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty(),
        "{response:?}"
    );
}
fn plugin(resolve: bool) -> Value {
    let callbacks = if resolve {
        vec![Value::object([
            ("id", Value::Int(0)),
            ("filter", Value::from(".*")),
            ("namespace", Value::from("")),
        ])]
    } else {
        vec![]
    };
    Value::Array(vec![Value::object([
        ("name", Value::from("lifecycle")),
        ("onStart", Value::Bool(true)),
        ("onEnd", Value::Bool(true)),
        ("onResolve", Value::Array(callbacks)),
        ("onLoad", Value::Array(vec![])),
    ])])
}
fn create(host: &mut Host, request: Value) {
    host.request(0, request);
    let response = host.response(0);
    assert_ok(&response);
    assert_eq!(
        response.as_object().unwrap().len(),
        2,
        "creation must not compile or return output files"
    );
}

#[test]
fn context_wire_result_and_on_end_diagnostics_round_trip() {
    let mut host = Host::start();
    let mut request = context(1);
    set(
        &mut request,
        "flags",
        flags(&[
            "--loader=base64",
            "--format=cjs",
            "--metafile",
            "--log-level=silent",
        ]),
    );
    set(
        &mut request,
        "stdinContents",
        Value::Bytes(vec![0xff, 0, 0xfe]),
    );
    create(&mut host, request);
    host.request(1, operation("rebuild", 1));
    let end = host.callback("on-end");
    assert_ok(&end.value);
    let output = &end.value.get("outputFiles").unwrap().as_array().unwrap()[0];
    assert_eq!(
        output.get("contents").and_then(Value::as_bytes),
        Some(b"module.exports = \"/wD+\";\n".as_slice())
    );
    assert!(
        end.value
            .get("metafile")
            .unwrap()
            .as_bytes()
            .unwrap()
            .starts_with(b"{")
    );
    let error = Value::object([
        ("id", Value::from("")),
        ("pluginName", Value::from("host-end")),
        ("text", Value::from("acknowledged error")),
        ("location", Value::Null),
        ("notes", Value::Array(vec![])),
        ("detail", Value::Int(-1)),
    ]);
    let mut ack = diagnostics();
    set(&mut ack, "errors", Value::Array(vec![error]));
    host.reply(end.id, ack);
    let response = host.response(1);
    assert!(response.get("outputFiles").is_none());
    assert_eq!(
        response.get("errors").unwrap().as_array().unwrap()[0]
            .get("text")
            .and_then(Value::as_str),
        Some("acknowledged error")
    );
    // Native onEnd errors cannot poison a later rebuild.
    host.request(2, operation("rebuild", 1));
    let end = host.callback("on-end");
    assert_ok(&end.value);
    host.reply(end.id, diagnostics());
    assert_ok(&host.response(2));
    host.dispose(1, 3);
    host.finish();
}

#[test]
fn context_dispose_waits_for_admitted_rebuilds_and_end_ack() {
    let mut host = Host::start();
    let mut request = context(7);
    set(&mut request, "plugins", plugin(false));
    create(&mut host, request);
    host.request(1, operation("rebuild", 7));
    let start = host.callback("on-start");
    host.request(2, operation("rebuild", 7));
    host.request(3, operation("dispose", 7));
    host.request(5, operation("rebuild", 7));
    assert_eq!(
        host.response(5).get("error").and_then(Value::as_str),
        Some("Cannot rebuild")
    );
    host.quiet();
    host.reply(start.id, diagnostics());
    let end = host.callback("on-end");
    host.quiet();
    host.reply(end.id, diagnostics());
    let mut replies = std::collections::BTreeMap::new();
    for _ in 0..3 {
        let packet = host.packet();
        assert!(!packet.is_request, "both native rebuilds must coalesce");
        replies.insert(packet.id, packet.value);
    }
    for id in [1, 2] {
        assert_ok(&replies.remove(&id).unwrap());
    }
    assert!(replies.remove(&3).unwrap().as_object().unwrap().is_empty());
    host.dispose(7, 6);
    host.finish();
}

#[test]
fn context_nested_resolve_retained_across_rebuilds_and_dispose_waits() {
    let mut host = Host::start();
    let mut request = context(8);
    set(&mut request, "plugins", plugin(true));
    create(&mut host, request);
    // The same native resolve capability lives across independent rebuilds.
    for rebuild in [1, 2] {
        host.request(rebuild, operation("rebuild", 8));
        let start = host.callback("on-start");
        let mut resolve = operation("resolve", 8);
        set(&mut resolve, "path", Value::from("external-module"));
        set(&mut resolve, "kind", Value::from("import-statement"));
        host.request(10 + rebuild, resolve);
        let callback = host.callback("on-resolve");
        assert_eq!(
            callback.value.get("path").and_then(Value::as_str),
            Some("external-module")
        );
        if rebuild == 2 {
            host.request(20, operation("dispose", 8));
        }
        host.quiet();
        host.reply(
            callback.id,
            Value::object([
                ("id", Value::Int(0)),
                ("path", Value::from("external-module")),
                ("external", Value::Bool(true)),
            ]),
        );
        let resolve = host.response(10 + rebuild);
        assert_ok(&resolve);
        assert_eq!(resolve.get("external").and_then(Value::as_bool), Some(true));
        host.reply(start.id, diagnostics());
        let end = host.callback("on-end");
        assert_ok(&end.value);
        host.reply(end.id, diagnostics());
        let first = host.packet();
        assert!(!first.is_request);
        if rebuild == 2 {
            let second = host.packet();
            let (done, disposed) = if first.id == 2 {
                (first, second)
            } else {
                (second, first)
            };
            assert_eq!(done.id, 2);
            assert_ok(&done.value);
            assert_eq!(disposed.id, 20);
            assert!(disposed.value.as_object().unwrap().is_empty());
        } else {
            assert_eq!(first.id, 1);
            assert_ok(&first.value);
        }
    }
    let mut resolve = operation("resolve", 8);
    set(&mut resolve, "path", Value::from("after-dispose"));
    host.request(21, resolve);
    assert_eq!(
        host.response(21).get("error").and_then(Value::as_str),
        Some("Cannot call \"resolve\" on an inactive build")
    );
    host.finish();
}

#[test]
fn context_cancel_waits_for_on_start_and_on_end_and_recovers() {
    let mut host = Host::start();
    let mut request = context(9);
    set(&mut request, "plugins", plugin(false));
    create(&mut host, request);
    host.request(1, operation("rebuild", 9));
    let start = host.callback("on-start");
    host.request(2, operation("cancel", 9));
    host.quiet();
    host.reply(start.id, diagnostics());
    let end = host.callback("on-end");
    assert_eq!(
        end.value.get("errors").unwrap().as_array().unwrap()[0]
            .get("text")
            .and_then(Value::as_str),
        Some("The build was canceled")
    );
    assert!(
        end.value
            .get("outputFiles")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    host.quiet();
    host.reply(end.id, diagnostics());
    let a = host.packet();
    let b = host.packet();
    let (build, cancel) = if a.id == 1 { (a, b) } else { (b, a) };
    assert_eq!(build.id, 1);
    assert_eq!(cancel.id, 2);
    assert!(cancel.value.as_object().unwrap().is_empty());
    assert_eq!(
        build.value.get("errors").unwrap().as_array().unwrap()[0]
            .get("text")
            .and_then(Value::as_str),
        Some("The build was canceled")
    );
    host.request(3, operation("rebuild", 9));
    let start = host.callback("on-start");
    host.reply(start.id, diagnostics());
    let end = host.callback("on-end");
    assert_ok(&end.value);
    host.reply(end.id, diagnostics());
    assert_ok(&host.response(3));
    host.dispose(9, 4);
    host.finish();
}

#[test]
fn context_resolve_in_paused_callback_remains_live_after_dispose_admission() {
    for held in ["on-start", "on-end"] {
        let mut host = Host::start();
        let mut request = context(13);
        set(&mut request, "plugins", plugin(true));
        create(&mut host, request);
        host.request(1, operation("rebuild", 13));
        let start = host.callback("on-start");
        let paused = if held == "on-end" {
            host.reply(start.id, diagnostics());
            host.callback("on-end")
        } else {
            start
        };
        host.request(2, operation("dispose", 13));
        // This independent round trip fences reader admission of dispose.
        host.request(
            3,
            Value::object([
                ("command", Value::from("transform")),
                ("inputFS", Value::Bool(false)),
                ("input", Value::Bytes(b"let barrier = 1".to_vec())),
                ("flags", flags(&["--log-level=silent"])),
            ]),
        );
        assert_ok(&host.response(3));
        let mut resolve = operation("resolve", 13);
        set(&mut resolve, "path", Value::from("nested"));
        set(&mut resolve, "kind", Value::from("import-statement"));
        host.request(4, resolve);
        let callback = host.callback("on-resolve");
        host.reply(
            callback.id,
            Value::object([
                ("id", Value::Int(0)),
                ("path", Value::from("nested")),
                ("external", Value::Bool(true)),
            ]),
        );
        let response = host.response(4);
        assert_ok(&response);
        assert_eq!(response.get("path").and_then(Value::as_str), Some("nested"));
        assert_eq!(
            response.get("external").and_then(Value::as_bool),
            Some(true)
        );
        host.reply(paused.id, diagnostics());
        if held == "on-start" {
            let end = host.callback("on-end");
            host.reply(end.id, diagnostics());
        }
        let first = host.packet();
        let second = host.packet();
        let (rebuilt, disposed) = if first.id == 1 {
            (first, second)
        } else {
            (second, first)
        };
        assert_eq!(rebuilt.id, 1);
        assert_ok(&rebuilt.value);
        assert_eq!(disposed.id, 2);
        assert!(disposed.value.as_object().unwrap().is_empty());
        let mut resolve = operation("resolve", 13);
        set(&mut resolve, "path", Value::from("after-dispose"));
        host.request(5, resolve);
        assert_eq!(
            host.response(5).get("error").and_then(Value::as_str),
            Some("Cannot call \"resolve\" on an inactive build")
        );
        host.finish();
    }
}

#[test]
fn context_wire_write_to_stdout_stays_framed() {
    let mut host = Host::start();
    let mut request = context(10);
    set(&mut request, "write", Value::Bool(true));
    create(&mut host, request);
    host.request(1, operation("rebuild", 10));
    let end = host.callback("on-end");
    assert!(end.value.get("outputFiles").is_none());
    assert_eq!(
        end.value.get("writeToStdout").and_then(Value::as_bytes),
        Some(b"const value = 1;\nexport {\n  value\n};\n".as_slice())
    );
    host.reply(end.id, diagnostics());
    assert_ok(&host.response(1));
    host.dispose(10, 2);
    host.finish();
}

#[test]
fn context_native_eof_closes_live_context_and_unanswered_callbacks() {
    // This explicitly tests the native service's host-disconnect cleanup.
    // Pinned Go's retained context keep-alive does not drain gracefully on EOF.
    for held in ["idle", "on-start", "on-end", "ping"] {
        let mut host = Host::start_with_ping(held == "ping");
        let mut request = context(11);
        set(&mut request, "plugins", plugin(false));
        create(&mut host, request);
        if held == "ping" {
            let _ = host.callback("ping");
        } else if held != "idle" {
            host.request(1, operation("rebuild", 11));
            let start = host.callback("on-start");
            if held == "on-end" {
                host.reply(start.id, diagnostics());
                let _ = host.callback("on-end");
            }
        }
        host.finish();
        if held == "on-start" || held == "on-end" {
            let response = host.response(1);
            assert!(
                !response
                    .get("errors")
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "EOF callback failure must reach the admitted rebuild response"
            );
        }
    }
}

#[test]
fn context_unknown_watch_and_unsupported_serve_return_errors() {
    let mut host = Host::start();
    create(&mut host, context(12));
    host.request(1, operation("watch", 999));
    assert_eq!(
        host.response(1).get("error").and_then(Value::as_str),
        Some("Cannot watch")
    );
    host.request(2, operation("serve", 12));
    assert!(
        host.response(2)
            .get("error")
            .and_then(Value::as_str)
            .unwrap()
            .contains("not implemented")
    );
    host.dispose(12, 3);
    host.finish();
}

#[test]
#[ignore = "reference-only EOF difference: run with pinned Go via ESBUILD_RS_SERVICE_CONTEXT_BINARY"]
fn context_go_eof_does_not_gracefully_drain_retained_contexts() {
    assert!(std::env::var_os("ESBUILD_RS_SERVICE_CONTEXT_BINARY").is_some());
    for held in ["idle", "on-start", "on-end"] {
        let mut host = Host::start();
        let mut request = context(11);
        set(&mut request, "plugins", plugin(false));
        create(&mut host, request);
        if held != "idle" {
            host.request(1, operation("rebuild", 11));
            let start = host.callback("on-start");
            if held == "on-end" {
                host.reply(start.id, diagnostics());
                let _ = host.callback("on-end");
            }
        }
        host.input.take();
        thread::sleep(Duration::from_millis(250));
        if let Some(status) = host.child.try_wait().unwrap() {
            let mut stderr = String::new();
            host.child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            eprintln!("Go EOF {held}: {status}; stderr={stderr}");
            assert!(
                !status.success(),
                "Go does not drain retained contexts gracefully"
            );
            assert!(
                stderr.contains("deadlock"),
                "pinned Go runtime detects the stranded keep-alive"
            );
        } else {
            eprintln!("Go EOF {held}: retained keep-alive still running after 250 ms");
        }
        // Drop kills any reference process still alive after this observation.
    }
}
