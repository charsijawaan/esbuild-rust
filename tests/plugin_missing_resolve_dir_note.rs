//! Diagnostics captured from the pinned Go API, including suppression controls.
use std::{fs, path::PathBuf};

use esbuild_rs::api::{
    AbsPaths, BuildFormat, BuildOptions, Loader, Location, LogLevel, Message, Note, OnLoadOptions,
    OnLoadResult, OnResolveOptions, OnResolveResult, Plugin, build,
};
use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "esbuild-rs-resolve-dir-note-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create fixture");
        fs::write(root.join("loadme.js"), "export default 123").expect("write dependency");
        Self(fs::canonicalize(root).expect("canonical fixture"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn text(input: &Value, key: &str) -> String {
    input[key].as_str().expect("string input").to_string()
}

fn plugins(input: &Value, fixture: &Fixture) -> Vec<Plugin> {
    let namespace = match input["namespace"].as_str().expect("namespace") {
        "" => "for-testing".to_string(),
        value => value.to_string(),
    };
    let path = match input["path"].as_str().expect("path") {
        "" => "virtual".to_string(),
        value => value.to_string(),
    };
    let path = if namespace == "file" {
        fixture.0.join(path).to_string_lossy().into_owned()
    } else {
        path
    };
    let filter = format!("^{}$", regex::escape(&path));
    let resolve_action = text(input, "resolve_action");
    let resolve_namespace = namespace.clone();
    let mut plugins = vec![Plugin::new("resolver", move |plugin| {
        let path = path.clone();
        let namespace = resolve_namespace.clone();
        plugin.on_resolve(
            OnResolveOptions {
                filter: "^virtual-entry$".into(),
                ..OnResolveOptions::default()
            },
            move |_| {
                Ok(OnResolveResult {
                    path: path.clone(),
                    namespace: namespace.clone(),
                    plugin_name: "resolve-attribution".into(),
                    ..OnResolveResult::default()
                })
            },
        );
        if !resolve_action.is_empty() {
            let external = resolve_action == "external";
            plugin.on_resolve(
                OnResolveOptions {
                    filter: "^\\./loadme$".into(),
                    namespace: resolve_namespace.clone(),
                },
                move |_| {
                    Ok(if external {
                        OnResolveResult {
                            external: true,
                            ..OnResolveResult::default()
                        }
                    } else {
                        OnResolveResult {
                            plugin_name: "reported-resolver".into(),
                            errors: vec![Message {
                                text: "custom failure".into(),
                                notes: vec![Note {
                                    text: "custom note".into(),
                                    ..Note::default()
                                }],
                                ..Message::default()
                            }],
                            ..OnResolveResult::default()
                        }
                    })
                },
            );
        }
        Ok(())
    })];
    let load_options = OnLoadOptions { filter, namespace };
    if input["deferred"] == true {
        let options = load_options.clone();
        plugins.push(Plugin::new("deferred", move |plugin| {
            plugin.on_load(options.clone(), |_| {
                Ok(OnLoadResult {
                    plugin_name: "unused-attribution".into(),
                    ..OnLoadResult::default()
                })
            });
            Ok(())
        }));
    }
    plugins.push(loader(input, fixture, load_options));
    plugins
}

fn loader(input: &Value, fixture: &Fixture, load_options: OnLoadOptions) -> Plugin {
    let contents = text(input, "contents");
    let plugin_name = text(input, "plugin_name");
    let resolve_dir = if input["resolve_dir"] == true {
        fixture.0.to_string_lossy().into_owned()
    } else {
        String::new()
    };
    let loader = if input["loader"] == "css" {
        Loader::Css
    } else {
        Loader::Js
    };
    Plugin::new("loader", move |plugin| {
        let contents = contents.clone();
        let plugin_name = plugin_name.clone();
        let resolve_dir = resolve_dir.clone();
        plugin.on_load(load_options.clone(), move |_| {
            Ok(OnLoadResult {
                contents: Some(contents.clone()),
                loader,
                plugin_name: plugin_name.clone(),
                resolve_dir: resolve_dir.clone(),
                ..OnLoadResult::default()
            })
        });
        Ok(())
    })
}

fn location_json(location: Option<&Location>, root: &str) -> Value {
    location.map_or(Value::Null, |location| {
        json!({
            "file": location.file.replace(root, "<fixture>"),
            "namespace": location.namespace,
            "line": location.line,
            "column": location.column,
            "length": location.length,
            "line_text": location.line_text,
            "suggestion": location.suggestion,
        })
    })
}

fn messages_json(messages: &[Message], fixture: &Fixture) -> Value {
    let root = fixture.0.to_string_lossy();
    Value::Array(
        messages
            .iter()
            .map(|message| {
                let notes: Vec<_> = message
                    .notes
                    .iter()
                    .map(|note| {
                        json!({
                            "text": note.text.replace(root.as_ref(), "<fixture>"),
                            "location": location_json(note.location.as_ref(), &root),
                        })
                    })
                    .collect();
                json!({
                    "id": message.id,
                    "plugin_name": message.plugin_name,
                    "text": message.text,
                    "location": location_json(message.location.as_ref(), &root),
                    "notes": notes,
                    "detail_is_nil": message.detail.is_none(),
                })
            })
            .collect(),
    )
}

#[test]
fn virtual_import_diagnostics_match_pinned_go_api() {
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "upstream/plugin_missing_resolve_dir_note.json"
    ))
    .expect("Go API fixture");
    assert_cases_match_go(&cases, "ESBUILD_RESOLVE_DIR_NOTE_REPORT");
}

#[test]
fn missing_resolve_directory_notes_quote_controls_and_combining_unicode_like_go() {
    let cases: Vec<Value> = serde_json::from_str(include_str!(
        "upstream/plugin_missing_resolve_dir_note_quotes.json"
    ))
    .expect("Go API quote fixture");
    assert_cases_match_go(&cases, "ESBUILD_RESOLVE_DIR_NOTE_QUOTES_REPORT");
}

fn assert_cases_match_go(cases: &[Value], report_environment: &str) {
    let fixture = Fixture::new();
    let mut observed = Vec::new();
    let mut failures = Vec::new();
    for expected in cases {
        let input = &expected["input"];
        let result = build(BuildOptions {
            abs_working_dir: fixture.0.to_string_lossy().into_owned(),
            entry_points: vec!["virtual-entry".into()],
            bundle: true,
            write: false,
            outfile: "out.js".into(),
            format: BuildFormat::CommonJs,
            log_override: [("ignored-dynamic-import".into(), LogLevel::Silent)].into(),
            abs_paths: if input["abs_paths"] == true {
                AbsPaths::LOG
            } else {
                AbsPaths::default()
            },
            plugins: plugins(input, &fixture),
            ..BuildOptions::default()
        });
        let actual = json!({
            "input": input,
            "errors": messages_json(&result.errors, &fixture),
            "warnings": messages_json(&result.warnings, &fixture),
            "output_files": result.output_files.len(),
        });
        if &actual != expected {
            failures.push(format!(
                "{}: expected {expected}, actual {actual}",
                input["name"]
            ));
        }
        observed.push(actual);
    }
    if let Some(path) = std::env::var_os(report_environment) {
        fs::write(
            path,
            serde_json::to_vec_pretty(&observed).expect("report JSON"),
        )
        .expect("write report");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
