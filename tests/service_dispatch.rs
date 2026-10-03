//! Process-level service tests. Original JS API assertions are selected by
//! `scripts/audit_upstream_service_tests.mjs`; these cover transport shutdown.

use std::{
    collections::BTreeMap,
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

use base64::Engine as _;
use esbuild_rs::service::{Packet, VERSION, Value, decode_packet, encode_packet};

struct Host {
    child: Child,
    input: Option<ChildStdin>,
    frames: mpsc::Receiver<io::Result<Vec<u8>>>,
}

impl Host {
    fn start(ping: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_esbuild"));
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

fn transform(bytes: Vec<u8>) -> Value {
    Value::object([
        ("command", Value::from("transform")),
        ("inputFS", Value::Bool(false)),
        ("input", Value::Bytes(bytes)),
        (
            "flags",
            Value::Array(vec![
                Value::from("--loader=base64"),
                Value::from("--log-level=silent"),
            ]),
        ),
    ])
}

fn set(request: &mut Value, name: &str, value: Value) {
    let Value::Object(object) = request else {
        panic!("expected request object")
    };
    object.insert(name.as_bytes().to_vec(), value);
}

fn build() -> Value {
    Value::object([
        ("command", Value::from("build")),
        ("context", Value::Bool(false)),
        ("key", Value::Int(1)),
        ("write", Value::Bool(true)),
        ("entries", Value::Array(vec![])),
        ("nodePaths", Value::Array(vec![])),
        ("absWorkingDir", Value::from("")),
        (
            "flags",
            Value::Array(vec![Value::from("--log-level=silent")]),
        ),
        ("stdinContents", Value::Bytes(b"let foo = 1".to_vec())),
    ])
}

#[test]
fn fragmented_concurrent_requests_drain_after_eof() {
    let mut host = Host::start(false);
    let mut expected = BTreeMap::new();
    for id in 0_u8..48 {
        let bytes = vec![0, id, 255];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        expected.insert(u32::from(id), format!("module.exports = {encoded:?};\n"));
        host.request(u32::from(id), transform(bytes));
    }
    host.eof();
    for _ in 0..48 {
        let packet = host.packet();
        assert!(!packet.is_request);
        assert_eq!(
            packet.value.get("code").and_then(Value::as_str),
            expected.remove(&packet.id).as_deref()
        );
        assert!(
            packet
                .value
                .get("errors")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    assert!(expected.is_empty());
    host.wait();
    assert!(host.frames.recv_timeout(Duration::from_secs(1)).is_err());
}

#[test]
fn ping_response_is_processed_before_eof_and_eof_unblocks_unanswered_ping() {
    for reply in [false, true] {
        let mut host = Host::start(true);
        let ping = host.packet();
        assert!(ping.is_request);
        assert_eq!(
            ping.value.get("command").and_then(Value::as_str),
            Some("ping")
        );
        if reply {
            host.send(&Packet {
                id: ping.id,
                is_request: false,
                value: Value::object([] as [(&str, Value); 0]),
            });
            host.request(9, transform(vec![255]));
            assert_eq!(host.packet().id, 9);
        }
        host.eof();
        host.wait();
    }
}

#[test]
fn build_stdout_is_returned_inside_response() {
    let mut host = Host::start(false);
    host.request(4, build());
    host.eof();
    let response = host.packet();
    assert_eq!(response.id, 4);
    assert!(response.value.get("outputFiles").is_none());
    assert_eq!(
        response
            .value
            .get("writeToStdout")
            .and_then(Value::as_bytes),
        Some(b"let foo = 1;\n".as_slice())
    );
    assert!(
        response
            .value
            .get("errors")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    host.wait();
}

#[test]
fn build_stdin_preserves_binary_and_present_empty_bytes() {
    let mut host = Host::start(false);
    for (id, bytes) in [(0, vec![0, 255, 128]), (1, vec![])] {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let mut request = build();
        set(&mut request, "write", Value::Bool(false));
        set(&mut request, "stdinContents", Value::Bytes(bytes));
        set(
            &mut request,
            "flags",
            Value::Array(vec![
                Value::from("--loader=base64"),
                Value::from("--log-level=silent"),
            ]),
        );
        host.request(id, request);
        let response = host.packet().value;
        assert!(
            response
                .get("errors")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        let files = response.get("outputFiles").unwrap().as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].get("contents").and_then(Value::as_bytes),
            Some(format!("module.exports = {encoded:?};\n").as_bytes())
        );
    }
    host.eof();
    host.wait();
}

#[test]
fn transform_files_preserve_binary_input_and_remove_temporary_input() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let directory = std::env::temp_dir().join(format!(
        "esbuild-service-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    let input = directory.join("input");
    fs::write(&input, [0, 255, 128]).unwrap();
    let mut request = transform(input.to_str().unwrap().as_bytes().to_vec());
    set(&mut request, "inputFS", Value::Bool(true));
    let mut host = Host::start(false);
    host.request(2, request);
    host.eof();
    let response = host.packet().value;
    assert_eq!(response.get("codeFS").and_then(Value::as_bool), Some(true));
    assert_eq!(response.get("mapFS").and_then(Value::as_bool), Some(false));
    assert!(!input.exists());
    let output = response.get("code").and_then(Value::as_str).unwrap();
    assert_eq!(fs::read(output).unwrap(), b"module.exports = \"AP+A\";\n");
    host.wait();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn unknown_context_watch_and_unsupported_serve_return_explicit_errors() {
    let mut host = Host::start(false);
    for (id, command) in (1..).zip(["watch", "serve"]) {
        host.request(
            id,
            Value::object([
                ("command", Value::from(command)),
                ("key", Value::Int(999)),
            ]),
        );
    }
    host.eof();
    for _ in 0..2 {
        let packet = host.packet();
        let response = packet.value;
        let error = response.get("error").and_then(Value::as_str).unwrap();
        if packet.id == 1 {
            assert_eq!(error, "Cannot watch");
        } else {
            assert_eq!(packet.id, 2);
            assert!(error.contains("not implemented yet"));
        }
        assert!(response.get("errors").is_none());
    }
    host.wait();
}

#[test]
fn rejects_incompatible_host_version_before_greeting() {
    for (version, quoted) in [
        ("0.0.0", "\"0.0.0\""),
        ("x\u{200c}", "\"x\\u200c\""),
        ("x\u{1}", "\"x\\x01\""),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .arg(format!("--service={version}"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr).unwrap().contains(&format!(
            "Host version {quoted} does not match binary version \"0.28.1\""
        )));
    }
}

#[test]
fn startup_scans_version_and_service_flags_in_upstream_order() {
    for flags in [
        vec!["--service=0.28.1", "--version"],
        vec!["--version", "--service=0.0.0"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(flags)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"0.28.1\n");
    }
    for flags in [
        vec!["--service=0.0.0", "--version"],
        vec!["--service=0.28.1", "--service=0.0.0"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(flags)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn error_logging_tolerates_flags_rejected_by_the_main_parser() {
    let mut host = Host::start(false);
    for (id, flag) in [(0, "--log-level=nope"), (1, "--log-limit=-1")] {
        host.request(
            id,
            Value::object([
                ("command", Value::from("error")),
                ("flags", Value::Array(vec![Value::from(flag)])),
                (
                    "error",
                    Value::object([
                        ("id", Value::from("")),
                        ("pluginName", Value::from("")),
                        ("text", Value::from("test service diagnostic")),
                        ("location", Value::Null),
                        ("notes", Value::Array(vec![])),
                        ("detail", Value::Int(-1)),
                    ]),
                ),
            ]),
        );
        let response = host.packet();
        assert_eq!(response.id, id);
        assert!(response.value.as_object().unwrap().is_empty());
    }
    host.eof();
    host.wait();
    let mut stderr = String::new();
    host.child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(stderr.matches("test service diagnostic").count(), 2);
}
