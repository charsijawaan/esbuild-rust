//! Stateless native stdin/stdout service dispatch.
//!
//! Source: pinned `cmd/esbuild/service.go`. Compilation runs independently of
//! the reader and writer; stdin EOF drains admitted jobs and their responses.

use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{api, internal::logger};

use super::{
    messages::{
        array_field, bool_field, decode_mangle_cache, decode_message, decode_messages,
        encode_mangle_cache, encode_messages, encode_output_files, internal_message, log_messages,
        string_array, text_field,
    },
    options::{parse_build_flags, parse_transform_flags},
    protocol::{FrameDecoder, Object, Packet, Value, decode_packet, encode_frame, encode_packet},
};

/// Wire/API version of the exact upstream revision in `UPSTREAM.md`.
pub const VERSION: &str = "0.28.1";

#[derive(Default)]
struct Callbacks {
    next_id: u32,
    pending: HashMap<u32, mpsc::Sender<Value>>,
}

struct Service {
    outgoing: mpsc::Sender<Vec<u8>>,
    callbacks: Mutex<Callbacks>,
    closed: Mutex<bool>,
    changed: Condvar,
}

impl Service {
    fn send_packet(&self, packet: &Packet) -> io::Result<()> {
        let bytes = encode_packet(packet).map_err(io::Error::other)?;
        self.outgoing
            .send(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "The service was stopped"))
    }

    fn send_request(&self, value: Value) -> io::Result<Value> {
        let (sender, receiver) = mpsc::channel();
        let id = {
            let mut callbacks = self
                .callbacks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *self
                .closed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "The service was stopped",
                ));
            }
            let id = callbacks.next_id & 0x7fff_ffff;
            callbacks.next_id = id.wrapping_add(1);
            callbacks.pending.insert(id, sender);
            id
        };
        if let Err(error) = self.send_packet(&Packet {
            id,
            is_request: true,
            value,
        }) {
            self.callbacks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .remove(&id);
            return Err(error);
        }
        receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "The service was stopped"))
    }

    fn receive_response(&self, packet: Packet) -> io::Result<()> {
        let callback = self
            .callbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(&packet.id);
        let Some(callback) = callback else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown service callback: {}", packet.id),
            ));
        };
        let _ = callback.send(packet.value);
        Ok(())
    }

    fn close(&self) {
        // Keep the same lock order as send_request so shutdown cannot deadlock.
        let mut callbacks = self
            .callbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *self
            .closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        callbacks.pending.clear();
        self.changed.notify_all();
    }

    fn ping_loop(&self) {
        loop {
            let closed = self
                .closed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (closed, _) = self
                .changed
                .wait_timeout_while(closed, Duration::from_secs(1), |closed| !*closed)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *closed {
                return;
            }
            drop(closed);
            if self
                .send_request(Value::object([("command", Value::from("ping"))]))
                .is_err()
            {
                return;
            }
        }
    }
}

/// Run the native service over the process's stdin/stdout.
///
/// # Errors
/// Returns framing/read failures after admitted stateless responses are drained.
/// A stdout write failure terminates the process, matching the pinned service.
pub fn run_service(send_pings: bool) -> io::Result<()> {
    logger::set_api_kind(logger::ApiKind::Js);
    run_with_io(io::stdin(), io::stdout(), send_pings, true)
}

fn join_jobs(jobs: &mut Vec<JoinHandle<()>>, all: bool) {
    let mut index = 0;
    while index < jobs.len() {
        if all || jobs[index].is_finished() {
            let _ = jobs.swap_remove(index).join();
        } else {
            index += 1;
        }
    }
}

fn run_with_io<R: Read, W: Write + Send + 'static>(
    mut input: R,
    mut output: W,
    send_pings: bool,
    exit_on_write_error: bool,
) -> io::Result<()> {
    let (outgoing, packets) = mpsc::channel::<Vec<u8>>();
    let writer = thread::spawn(move || -> io::Result<()> {
        for bytes in packets {
            if let Err(error) = output.write_all(&bytes).and_then(|()| output.flush()) {
                if exit_on_write_error {
                    std::process::exit(1);
                }
                return Err(error);
            }
        }
        output.flush()
    });
    let service = Arc::new(Service {
        outgoing,
        callbacks: Mutex::new(Callbacks::default()),
        closed: Mutex::new(false),
        changed: Condvar::new(),
    });
    service
        .outgoing
        .send(encode_frame(VERSION.as_bytes()).map_err(io::Error::other)?)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "The service was stopped"))?;
    let pinger = send_pings.then(|| {
        let service = service.clone();
        thread::spawn(move || service.ping_loop())
    });
    let mut frames = FrameDecoder::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut jobs = Vec::new();
    let read_result = (|| {
        loop {
            let count = match input.read(&mut buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                return Ok(());
            }
            frames.push(&buffer[..count]);
            while let Some(bytes) = frames.next_frame() {
                let Ok(packet) = decode_packet(&bytes) else {
                    // The pinned Go service drops packets rejected by decodePacket.
                    continue;
                };
                if !packet.is_request {
                    service.receive_response(packet)?;
                    continue;
                }
                let service = service.clone();
                jobs.push(thread::spawn(move || {
                    let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_request(&packet.value)
                    }))
                    .unwrap_or_else(|_| {
                        Err("Internal error: native service request panicked".into())
                    });
                    let value = value.unwrap_or_else(error_response);
                    let _ = service.send_packet(&Packet {
                        id: packet.id,
                        is_request: false,
                        value,
                    });
                }));
                join_jobs(&mut jobs, false);
            }
        }
    })();
    service.close();
    if let Some(pinger) = pinger {
        let _ = pinger.join();
    }
    join_jobs(&mut jobs, true);
    drop(service);
    let write_result = writer
        .join()
        .map_err(|_| io::Error::other("Service writer panicked"))?;
    read_result.and(write_result)
}

fn error_response(error: String) -> Value {
    Value::object([("error", Value::from(error))])
}

fn empty_response() -> Value {
    Value::Object(Object::new())
}

fn request_flags(request: &Value) -> Result<Vec<String>, String> {
    string_array(array_field(request, "flags")?)
}

fn handle_request(request: &Value) -> Result<Value, String> {
    let command = text_field(request, "command")?;
    match command.as_str() {
        "transform" => handle_transform(request),
        "build" => handle_build(request),
        "error" => {
            let flags = request_flags(request)?;
            let message = decode_message(
                request
                    .get("error")
                    .ok_or_else(|| "Missing service error message".to_string())?,
                api::MessageKind::Error,
            )?;
            logger::print_message_to_stderr(&flags, internal_message(message));
            Ok(empty_response())
        }
        "format-msgs" => {
            let kind = if bool_field(request, "isWarning")? {
                api::MessageKind::Warning
            } else {
                api::MessageKind::Error
            };
            let messages = decode_messages(array_field(request, "messages")?, kind)?;
            let result = api::format_messages(
                messages,
                api::FormatMessagesOptions {
                    kind,
                    color: request
                        .get("color")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    terminal_width: request
                        .get("terminalWidth")
                        .and_then(Value::as_int)
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or_default(),
                },
            );
            Ok(Value::object([(
                "messages",
                Value::Array(result.into_iter().map(Value::from).collect()),
            )]))
        }
        "analyze-metafile" => {
            let result = api::analyze_metafile(
                &text_field(request, "metafile")?,
                api::AnalyzeMetafileOptions {
                    color: request
                        .get("color")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    verbose: request
                        .get("verbose")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
            );
            Ok(Value::object([("result", Value::from(result))]))
        }
        "rebuild" | "watch" | "cancel" | "dispose" | "resolve" | "serve" => Err(format!(
            "The {command:?} command is not implemented yet by the native service"
        )),
        _ => Err(format!("Invalid command: {command}")),
    }
}

fn handle_transform(request: &Value) -> Result<Value, String> {
    let input_fs = bool_field(request, "inputFS")?;
    let input = request
        .get("input")
        .and_then(Value::as_bytes)
        .ok_or_else(|| "Invalid service request: transform input must be bytes".to_string())?;
    let parsed = parse_transform_flags(&request_flags(request)?)?;
    let mut options = parsed.options;
    options.mangle_cache = decode_mangle_cache(request.get("mangleCache"))?;
    let input_path = if input_fs {
        Some(
            std::str::from_utf8(input)
                .map_err(|_| "Transform input path must be valid UTF-8".to_string())?,
        )
    } else {
        None
    };
    let source = if let Some(path) = input_path {
        let contents = fs::read(path).map_err(|error| error.to_string())?;
        fs::remove_file(path).map_err(|error| error.to_string())?;
        contents
    } else {
        input.to_vec()
    };
    let result = api::transform(&source, options);
    log_messages(&parsed.log_settings, &result.errors, &result.warnings);
    let mut response = Object::from([
        (b"errors".to_vec(), encode_messages(&result.errors)),
        (b"warnings".to_vec(), encode_messages(&result.warnings)),
    ]);
    for (name, mut contents) in [("code", result.code), ("map", result.map)] {
        let mut on_disk = false;
        if let Some(path) = input_path.filter(|_| !contents.is_empty()) {
            let path = format!("{path}.{name}");
            if fs::write(&path, &contents).is_ok() {
                contents = path.into_bytes();
                on_disk = true;
            }
        }
        response.insert(name.as_bytes().to_vec(), Value::String(contents));
        response.insert(format!("{name}FS").into_bytes(), Value::Bool(on_disk));
    }
    if !result.legal_comments.is_empty() {
        response.insert(
            b"legalComments".to_vec(),
            Value::String(result.legal_comments),
        );
    }
    if result.mangle_cache.is_some() {
        response.insert(
            b"mangleCache".to_vec(),
            encode_mangle_cache(result.mangle_cache.as_ref()),
        );
    }
    Ok(Value::Object(response))
}

fn handle_build(request: &Value) -> Result<Value, String> {
    if bool_field(request, "context")? {
        return Err("The context API is not implemented yet by the native service".into());
    }
    if request.get("plugins").is_some_and(|plugins| {
        !matches!(plugins, Value::Null)
            && plugins.as_array().is_none_or(|plugins| !plugins.is_empty())
    }) {
        return Err("JavaScript plugins are not implemented yet by the native service".into());
    }
    let write = bool_field(request, "write")?;
    let parsed = parse_build_flags(&request_flags(request)?)?;
    let mut options = parsed.options;
    options.abs_working_dir = text_field(request, "absWorkingDir")?;
    options.node_paths = string_array(array_field(request, "nodePaths")?)?;
    options.mangle_cache = decode_mangle_cache(request.get("mangleCache"))?;
    for entry in array_field(request, "entries")? {
        let entry = entry
            .as_array()
            .filter(|entry| entry.len() == 2)
            .ok_or_else(|| "Invalid service entry point: expected [output, input]".to_string())?;
        let entry = string_array(entry)?;
        options.entry_points_advanced.push(api::BuildEntryPoint {
            output_path: entry[0].clone(),
            input_path: entry[1].clone(),
        });
    }
    if let Some(contents) = request.get("stdinContents").and_then(Value::as_bytes) {
        let stdin = options.stdin.get_or_insert_with(api::BuildStdin::default);
        stdin.contents_bytes = Some(contents.to_vec());
        if let Some(resolve_dir) = request.get("stdinResolveDir").and_then(Value::as_str) {
            resolve_dir.clone_into(&mut stdin.resolve_dir);
        }
    }
    let write_to_stdout = write && options.outfile.is_empty() && options.outdir.is_empty();
    let metafile = options.metafile;
    let mangle_cache = options.mangle_cache.is_some();
    options.write = write && !write_to_stdout;
    let result = api::build(options);
    log_messages(&parsed.log_settings, &result.errors, &result.warnings);
    let mut response = Object::from([
        (b"errors".to_vec(), encode_messages(&result.errors)),
        (b"warnings".to_vec(), encode_messages(&result.warnings)),
    ]);
    if metafile {
        response.insert(
            b"metafile".to_vec(),
            Value::Bytes(result.metafile.into_bytes()),
        );
    }
    if mangle_cache {
        response.insert(
            b"mangleCache".to_vec(),
            encode_mangle_cache(result.mangle_cache.as_ref()),
        );
    }
    if write_to_stdout && result.output_files.len() == 1 {
        response.insert(
            b"writeToStdout".to_vec(),
            Value::Bytes(result.output_files[0].contents.clone()),
        );
    }
    if !write {
        response.insert(
            b"outputFiles".to_vec(),
            encode_output_files(result.output_files),
        );
    }
    Ok(Value::Object(response))
}
