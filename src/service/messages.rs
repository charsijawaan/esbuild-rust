//! Service translations for native API messages and output files.
//!
//! Source: pinned `cmd/esbuild/service.go:1291-1430`. Wire strings retain bytes;
//! JavaScript-owned details are opaque stash indices, not serialized objects.

use std::{collections::HashMap, sync::Arc};

use crate::{api, internal::logger};

use super::{options::LogSettings, protocol::Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WireIndex(u32);

pub(super) fn decode_plugin_data(value: Option<&Value>) -> Result<Option<api::PluginData>, String> {
    let Some(value) = value.filter(|value| !matches!(value, Value::Null)) else {
        return Ok(None);
    };
    let index = value
        .as_int()
        .ok_or_else(|| "Invalid service pluginData: expected an opaque integer".to_string())?;
    let index = u32::try_from(index & i64::from(u32::MAX)).expect("index is masked to 32 bits");
    Ok(Some(Arc::new(WireIndex(index))))
}

pub(super) fn encode_plugin_data(value: Option<&api::PluginData>) -> Value {
    value
        .and_then(|value| value.downcast_ref::<WireIndex>())
        .map_or(Value::Null, |index| Value::Int(i64::from(index.0)))
}

pub(super) fn text_field(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Invalid service request: expected {key:?} to be a UTF-8 string"))
}

pub(super) fn bool_field(value: &Value, key: &str) -> Result<bool, String> {
    value
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("Invalid service request: expected {key:?} to be a boolean"))
}

pub(super) fn array_field<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Invalid service request: expected {key:?} to be an array"))
}

pub(super) fn string_array(value: &[Value]) -> Result<Vec<String>, String> {
    value
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "Invalid service request: expected a UTF-8 string array".into())
        })
        .collect()
}

fn number_field(value: &Value, key: &str) -> Result<usize, String> {
    value
        .get(key)
        .and_then(Value::as_int)
        .and_then(|number| usize::try_from(number).ok())
        .ok_or_else(|| {
            format!("Invalid service request: expected {key:?} to be a nonnegative integer")
        })
}

fn encode_location(location: Option<&api::Location>) -> Value {
    location.map_or(Value::Null, |location| {
        Value::object([
            ("file", Value::from(location.file.clone())),
            ("namespace", Value::from(location.namespace.clone())),
            ("line", wire_number(location.line)),
            ("column", wire_number(location.column)),
            ("length", wire_number(location.length)),
            ("lineText", Value::from(location.line_text.clone())),
            ("suggestion", Value::from(location.suggestion.clone())),
        ])
    })
}

fn wire_number(value: usize) -> Value {
    // Integers on this protocol carry exactly the low 32 bits, matching Go.
    Value::Int(i64::from(u32::from_le_bytes(
        value.to_le_bytes()[..4]
            .try_into()
            .expect("usize is at least 32 bits"),
    )))
}

pub(super) fn encode_messages(messages: &[api::Message]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| {
                let detail = message
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.downcast_ref::<WireIndex>())
                    .map_or(-1, |index| i64::from(index.0));
                let notes = message
                    .notes
                    .iter()
                    .map(|note| {
                        Value::object([
                            ("text", Value::from(note.text.clone())),
                            ("location", encode_location(note.location.as_ref())),
                        ])
                    })
                    .collect();
                Value::object([
                    ("id", Value::from(message.id.clone())),
                    ("pluginName", Value::from(message.plugin_name.clone())),
                    ("text", Value::from(message.text.clone())),
                    ("location", encode_location(message.location.as_ref())),
                    ("notes", Value::Array(notes)),
                    ("detail", Value::Int(detail)),
                ])
            })
            .collect(),
    )
}

fn decode_location(value: &Value) -> Result<Option<api::Location>, String> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let namespace = text_field(value, "namespace")?;
    Ok(Some(api::Location {
        file: text_field(value, "file")?,
        namespace: if namespace.is_empty() {
            "file".into()
        } else {
            namespace
        },
        line: number_field(value, "line")?,
        column: number_field(value, "column")?,
        length: number_field(value, "length")?,
        line_text: text_field(value, "lineText")?,
        suggestion: text_field(value, "suggestion")?,
    }))
}

pub(super) fn decode_message(
    value: &Value,
    kind: api::MessageKind,
) -> Result<api::Message, String> {
    let location = value
        .get("location")
        .ok_or_else(|| "Invalid service message: missing location".to_string())?;
    let detail = u32::try_from(
        value.get("detail").and_then(Value::as_int).unwrap_or(-1) & i64::from(u32::MAX),
    )
    .expect("detail is masked to 32 bits");
    let notes = array_field(value, "notes")?
        .iter()
        .map(|note| {
            Ok(api::Note {
                text: text_field(note, "text")?,
                location: decode_location(
                    note.get("location")
                        .ok_or_else(|| "Invalid service note: missing location".to_string())?,
                )?,
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(api::Message {
        id: text_field(value, "id")?,
        plugin_name: text_field(value, "pluginName")?,
        text: text_field(value, "text")?,
        location: decode_location(location)?,
        notes,
        detail: (detail != u32::MAX).then(|| Arc::new(WireIndex(detail)) as api::PluginData),
        kind,
    })
}

pub(super) fn decode_messages(
    values: &[Value],
    kind: api::MessageKind,
) -> Result<Vec<api::Message>, String> {
    values
        .iter()
        .map(|value| decode_message(value, kind))
        .collect()
}

pub(super) fn encode_output_files(files: Vec<api::BuildOutputFile>) -> Value {
    Value::Array(
        files
            .into_iter()
            .map(|file| {
                Value::object([
                    ("path", Value::from(file.path)),
                    ("contents", Value::Bytes(file.contents)),
                    ("hash", Value::from(file.hash)),
                ])
            })
            .collect(),
    )
}

fn wire_json(value: &Value) -> Result<serde_json::Value, String> {
    Ok(match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Int(value) => serde_json::Value::Number((*value).into()),
        Value::String(_) => serde_json::Value::String(
            value
                .as_str()
                .ok_or_else(|| "Invalid service cache: expected a UTF-8 string".to_string())?
                .to_owned(),
        ),
        Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(wire_json).collect::<Result<_, _>>()?)
        }
        Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let key = String::from_utf8(key.clone())
                        .map_err(|_| "Invalid service cache key".to_string())?;
                    Ok((key, wire_json(value)?))
                })
                .collect::<Result<_, String>>()?,
        ),
        Value::Bytes(_) => {
            return Err("Invalid service cache: byte arrays are not cache values".into());
        }
    })
}

pub(super) fn decode_mangle_cache(
    value: Option<&Value>,
) -> Result<Option<api::MangleCache>, String> {
    let Some(value) = value else { return Ok(None) };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| "Invalid service mangle cache: expected an object".to_string())?;
    object
        .iter()
        .map(|(key, value)| {
            let key = String::from_utf8(key.clone())
                .map_err(|_| "Invalid service cache key".to_string())?;
            Ok((key, wire_json(value)?))
        })
        .collect::<Result<_, String>>()
        .map(Some)
}

pub(super) fn encode_mangle_cache(cache: Option<&api::MangleCache>) -> Value {
    cache.map_or(Value::Null, |cache| {
        Value::object(cache.iter().map(|(key, value)| {
            let value = value.as_str().map_or(Value::Bool(false), Value::from);
            (key, value)
        }))
    })
}

fn internal_location(location: api::Location) -> logger::MsgLocation {
    logger::MsgLocation {
        file: logger::PrettyPaths {
            abs: location.file.clone(),
            rel: location.file,
        },
        namespace: if location.namespace.is_empty() {
            "file".into()
        } else {
            location.namespace
        },
        line_text: location.line_text.into_bytes(),
        suggestion: location.suggestion,
        line: location.line,
        column: location.column,
        length: location.length,
    }
}

pub(super) fn internal_message(message: api::Message) -> logger::Msg {
    logger::Msg {
        id: logger::string_to_maximum_msg_id(&message.id),
        plugin_name: message.plugin_name,
        kind: match message.kind {
            api::MessageKind::Error => logger::MsgKind::Error,
            api::MessageKind::Warning => logger::MsgKind::Warning,
        },
        data: logger::MsgData {
            text: message.text,
            location: message.location.map(internal_location),
            user_detail: message.detail,
            ..logger::MsgData::default()
        },
        notes: message
            .notes
            .into_iter()
            .map(|note| logger::MsgData {
                text: note.text,
                location: note.location.map(internal_location),
                ..logger::MsgData::default()
            })
            .collect(),
    }
}

fn internal_level(level: api::LogLevel) -> logger::LogLevel {
    match level {
        api::LogLevel::Silent => logger::LogLevel::Silent,
        api::LogLevel::Verbose => logger::LogLevel::Verbose,
        api::LogLevel::Debug => logger::LogLevel::Debug,
        api::LogLevel::Info => logger::LogLevel::Info,
        api::LogLevel::Warning => logger::LogLevel::Warning,
        api::LogLevel::Error => logger::LogLevel::Error,
    }
}

pub(super) fn log_messages(
    settings: &LogSettings,
    errors: &[api::Message],
    warnings: &[api::Message],
) {
    let mut overrides = HashMap::new();
    for (name, level) in &settings.overrides {
        logger::string_to_msg_ids(name, internal_level(*level), &mut overrides);
    }
    let log = logger::new_stderr_log(logger::OutputOptions {
        message_limit: settings.limit,
        include_source: true,
        color: match settings.color {
            None => logger::UseColor::IfTerminal,
            Some(false) => logger::UseColor::Never,
            Some(true) => logger::UseColor::Always,
        },
        log_level: internal_level(settings.level),
        overrides,
        ..logger::OutputOptions::default()
    });
    for message in errors.iter().chain(warnings) {
        log.add_msg(internal_message(message.clone()));
    }
    let _ = log.done();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_message_details_and_byte_locations_round_trip() {
        let original = api::Message {
            id: "test-id".into(),
            plugin_name: "plugin".into(),
            text: "message".into(),
            location: Some(api::Location {
                file: "file.js".into(),
                namespace: "file".into(),
                column: 3,
                length: 2,
                line: 1,
                line_text: "π x".into(),
                ..api::Location::default()
            }),
            notes: vec![api::Note {
                text: "note".into(),
                location: None,
            }],
            detail: Some(Arc::new(WireIndex(42))),
            kind: api::MessageKind::Error,
        };
        let wire = encode_messages(&[original]);
        let decoded =
            decode_message(&wire.as_array().unwrap()[0], api::MessageKind::Error).unwrap();
        assert_eq!(
            decoded.detail.unwrap().downcast_ref::<WireIndex>(),
            Some(&WireIndex(42))
        );
        assert_eq!(decoded.location.unwrap().column, 3);
        assert_eq!(decoded.notes[0].text, "note");
        let without_detail = encode_messages(&[api::Message::default()]);
        let bytes = super::super::protocol::encode_packet(&super::super::protocol::Packet {
            id: 0,
            is_request: false,
            value: without_detail,
        })
        .unwrap();
        let packet = super::super::protocol::decode_packet(&bytes[4..]).unwrap();
        let message = &packet.value.as_array().unwrap()[0];
        assert_eq!(
            message.get("detail").and_then(Value::as_int),
            Some(i64::from(u32::MAX))
        );
        assert!(
            decode_message(message, api::MessageKind::Error)
                .unwrap()
                .detail
                .is_none()
        );
    }

    #[test]
    fn absent_empty_and_reserved_mangle_caches_remain_distinct() {
        assert!(decode_mangle_cache(None).unwrap().is_none());
        assert!(
            decode_mangle_cache(Some(&Value::object([] as [(&str, Value); 0])))
                .unwrap()
                .unwrap()
                .is_empty()
        );
        let wire = Value::object([("x_", Value::from("fixed")), ("z_", Value::Bool(false))]);
        let cache = decode_mangle_cache(Some(&wire)).unwrap().unwrap();
        assert_eq!(encode_mangle_cache(Some(&cache)), wire);
    }
}
