//! Original Go API helper expectations, captured without rewriting assertions.
use serde_json::Value;

use super::{FormatMessagesOptions, Location, Message, MessageKind, Note};

fn check_keys(value: &Value, keys: &[&str]) {
    for key in value.as_object().expect("upstream object").keys() {
        assert!(
            keys.contains(&key.as_str()),
            "unmapped upstream API field {key}"
        );
    }
}

fn string(value: &Value, key: &str) -> String {
    value[key].as_str().expect("upstream string").into()
}

fn number(value: &Value, key: &str) -> usize {
    value[key]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .expect("upstream non-negative integer")
}

fn location(value: &Value) -> Option<Location> {
    if value.is_null() {
        return None;
    }
    check_keys(
        value,
        &[
            "File",
            "Namespace",
            "Line",
            "Column",
            "Length",
            "LineText",
            "Suggestion",
        ],
    );
    Some(Location {
        file: string(value, "File"),
        namespace: string(value, "Namespace"),
        line: number(value, "Line"),
        column: number(value, "Column"),
        length: number(value, "Length"),
        line_text: string(value, "LineText"),
        suggestion: string(value, "Suggestion"),
    })
}

#[test]
fn matches_pinned_upstream_go_api_corpus() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../../tests/upstream/api.json")).unwrap();
    assert_eq!(cases.len(), 33, "upstream Go API case count changed");
    let mut failures = Vec::new();
    for case in cases {
        match case["kind"].as_str().unwrap() {
            "strip_dir_prefix" => {
                check_keys(
                    &case,
                    &[
                        "allowed_slashes",
                        "expected",
                        "file",
                        "kind",
                        "line",
                        "path",
                        "prefix",
                        "success",
                        "upstream_test",
                    ],
                );
                let actual = super::strip_dir_prefix(
                    case["path"].as_str().unwrap(),
                    case["prefix"].as_str().unwrap(),
                    case["allowed_slashes"].as_str().unwrap(),
                );
                let expected = if case["success"].as_bool().unwrap() {
                    Some(case["expected"].as_str().unwrap())
                } else {
                    None
                };
                if actual != expected {
                    failures.push(format!(
                        "{}:{}: expected {expected:?}, actual {actual:?}",
                        case["file"], case["line"]
                    ));
                }
            }
            "format" => {
                check_keys(
                    &case,
                    &[
                        "expected",
                        "file",
                        "kind",
                        "line",
                        "message",
                        "name",
                        "options",
                        "upstream_test",
                    ],
                );
                let options = &case["options"];
                check_keys(options, &["TerminalWidth", "Kind", "Color"]);
                let kind = match options["Kind"].as_u64().unwrap() {
                    0 => MessageKind::Error,
                    1 => MessageKind::Warning,
                    _ => panic!("unmapped message kind"),
                };
                let msg = &case["message"];
                check_keys(
                    msg,
                    &["ID", "PluginName", "Text", "Location", "Notes", "Detail"],
                );
                assert!(msg["Detail"].is_null(), "unmapped message detail");
                let notes = if msg["Notes"].is_null() {
                    Vec::new()
                } else {
                    msg["Notes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|note| {
                            check_keys(note, &["Text", "Location"]);
                            Note {
                                text: string(note, "Text"),
                                location: location(&note["Location"]),
                            }
                        })
                        .collect()
                };
                let actual = super::format_messages(
                    vec![Message {
                        id: string(msg, "ID"),
                        plugin_name: string(msg, "PluginName"),
                        text: string(msg, "Text"),
                        location: location(&msg["Location"]),
                        notes,
                        kind,
                        ..Message::default()
                    }],
                    FormatMessagesOptions {
                        terminal_width: number(options, "TerminalWidth"),
                        kind,
                        color: options["Color"].as_bool().unwrap(),
                    },
                );
                let expected = case["expected"].as_str().unwrap();
                if actual != [expected] {
                    failures.push(format!(
                        "{}:{} {}: expected {expected:?}, actual {actual:?}",
                        case["file"], case["line"], case["name"]
                    ));
                }
            }
            _ => panic!("unknown upstream API case"),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
