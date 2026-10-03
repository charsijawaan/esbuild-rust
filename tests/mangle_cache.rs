use std::{collections::HashMap, process::Command};

use esbuild_rs::api::{
    BuildOptions, BuildStdin, Loader, MangleCache, OnEndResult, Plugin, TransformOptions, build,
    context, transform,
};
use serde_json::{Value, json};

fn node(code: &[u8]) -> Vec<u8> {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("execute mangled code");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn options(source: &str, cache: Option<MangleCache>) -> BuildOptions {
    BuildOptions {
        stdin: Some(BuildStdin {
            contents: source.into(),
            loader: Loader::Js,
            ..BuildStdin::default()
        }),
        mangle_props: "_$".into(),
        mangle_cache: cache,
        tsconfig_raw: "{}".into(),
        ..BuildOptions::default()
    }
}

#[test]
fn api_property_caches_preserve_replacements_reservations_and_unused_entries() {
    let initial = HashMap::from([
        ("foo_".into(), json!("fixed")),
        ("keep_".into(), json!(false)),
        ("unused_".into(), json!("a")),
    ]);
    let source = "const object = { foo_: 1, keep_: 2, new_: 3 }; console.log(object.foo_ + object.keep_ + object.new_);";
    for minify in [false, true] {
        let mut build_options = options(source, Some(initial.clone()));
        build_options.minify_identifiers = minify;
        let result = build(build_options);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let cache = result.mangle_cache.expect("returned build cache");
        for (key, value) in &initial {
            assert_eq!(&cache[key], value);
        }
        assert_ne!(cache["new_"], json!("a"));
        assert_eq!(node(&result.output_files[0].contents), b"6\n");
        let result = transform(
            source,
            TransformOptions {
                mangle_props: "_$".into(),
                mangle_cache: Some(initial.clone()),
                minify_identifiers: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.mangle_cache, Some(cache));
        assert_eq!(node(&result.code), b"6\n");
    }
}

#[test]
fn returned_caches_can_be_reused_and_nil_differs_from_empty() {
    let first = transform(
        "console.log(({ foo_: 1 }).foo_)",
        TransformOptions {
            mangle_props: "_$".into(),
            mangle_cache: Some(MangleCache::new()),
            ..TransformOptions::default()
        },
    );
    let first_cache = first.mangle_cache.expect("first cache");
    let second = build(options(
        "console.log(({ foo_: 2, bar_: 3 }).foo_ + ({ bar_: 3 }).bar_)",
        Some(first_cache.clone()),
    ));
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    let cache = second.mangle_cache.expect("second cache");
    assert_eq!(cache["foo_"], first_cache["foo_"]);
    assert_ne!(cache["bar_"], cache["foo_"]);
    assert_eq!(node(&second.output_files[0].contents), b"5\n");
    assert!(build(options("foo()", None)).mangle_cache.is_none());
    assert_eq!(
        build(options("foo()", Some(MangleCache::new()))).mangle_cache,
        Some(MangleCache::new())
    );
    for loader in [Loader::Js, Loader::Css, Loader::Json, Loader::Text] {
        let source = match loader {
            Loader::Css => "a {}",
            Loader::Json => "{}",
            _ => "",
        };
        let result = transform(
            source,
            TransformOptions {
                loader,
                mangle_cache: Some(first_cache.clone()),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.mangle_cache, Some(first_cache.clone()));
    }
}

#[test]
fn caches_are_shared_across_separate_entry_point_compilations() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "esbuild-rs-entry-cache-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("a.js"),
        "console.log(({ first_: 1 }).first_)",
    )
    .unwrap();
    std::fs::write(
        directory.join("b.js"),
        "console.log(({ second_: 2 }).second_)",
    )
    .unwrap();
    let result = build(BuildOptions {
        abs_working_dir: directory.to_string_lossy().into_owned(),
        entry_points: vec!["a.js".into(), "b.js".into()],
        outdir: "out".into(),
        mangle_props: "_$".into(),
        mangle_cache: Some(MangleCache::new()),
        ..BuildOptions::default()
    });
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let cache = result.mangle_cache.expect("combined entry cache");
    assert_ne!(cache["first_"], cache["second_"]);
    assert_eq!(result.output_files.len(), 2);
    for output in result.output_files {
        assert_eq!(
            node(&output.contents),
            if output.path.ends_with("a.js") {
                b"1\n"
            } else {
                b"2\n"
            }
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_caches_fail_builds_and_transforms_but_allow_context_creation() {
    for value in [
        json!(true),
        Value::Null,
        json!(1),
        json!([]),
        json!({}),
        json!("__proto__"),
    ] {
        let cache = MangleCache::from([("bad_".into(), value.clone())]);
        let expected = if value == json!("__proto__") {
            "Invalid identifier name \"bad_\" in mangle cache"
        } else {
            "Expected \"bad_\" in mangle cache to map to either a string or false"
        };
        let result = build(options("foo()", Some(cache.clone())));
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].text, expected);
        assert!(result.mangle_cache.is_none());
        assert!(result.output_files.is_empty());
        let result = transform(
            "invalid ! code",
            TransformOptions {
                mangle_cache: Some(cache.clone()),
                ..TransformOptions::default()
            },
        );
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].text, expected);
        assert!(result.mangle_cache.is_none());
        assert!(result.code.is_empty());
        let syntax = build(options("const x = ;", Some(cache.clone())));
        assert_eq!(syntax.errors.len(), 1);
        assert!(!syntax.errors[0].text.contains("mangle cache"));
        assert!(syntax.mangle_cache.is_none());
        let build_context = context(options("foo()", Some(cache))).expect("defer cache validation");
        let result = build_context.rebuild();
        assert_eq!(result.errors[0].text, expected);
        assert!(result.mangle_cache.is_none());
        build_context.dispose();
    }
}

#[test]
fn compilation_errors_clear_caches_while_end_callbacks_receive_completed_caches() {
    let result = build(options("const x = ;", Some(MangleCache::new())));
    assert!(!result.errors.is_empty());
    assert!(result.mangle_cache.is_none());
    let mut build_options = options("console.log(({ foo_: 1 }).foo_)", Some(MangleCache::new()));
    build_options
        .plugins
        .push(Plugin::new("end-error", |build| {
            build.on_end(|result| {
                assert!(
                    result
                        .mangle_cache
                        .as_ref()
                        .is_some_and(|cache| cache.contains_key("foo_"))
                );
                Ok(OnEndResult {
                    errors: vec![esbuild_rs::api::Message {
                        text: "end failed".into(),
                        ..esbuild_rs::api::Message::default()
                    }],
                    ..OnEndResult::default()
                })
            });
            Ok(())
        }));
    let result = build(build_options);
    assert_eq!(result.errors[0].text, "end failed");
    assert!(
        result
            .mangle_cache
            .as_ref()
            .is_some_and(|cache| cache.contains_key("foo_"))
    );
}

#[test]
fn cli_cache_files_preserve_order_unicode_and_existing_reservations() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "esbuild-rs-cli-cache-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("in.js"), "foo(bar.baz)").unwrap();
    for utf8 in [false, true] {
        let alpha = if utf8 { "α" } else { "\\u03B1" };
        let cases = [
            (
                None,
                "{\n  \"baz\": \"a\"\n}\n".to_string(),
                "foo(bar.a);\n",
            ),
            (
                Some("{}"),
                "{\n  \"baz\": \"a\"\n}\n".to_string(),
                "foo(bar.a);\n",
            ),
            (
                Some("{\"baz\":false}"),
                "{\n  \"baz\": false\n}\n".to_string(),
                "foo(bar.baz);\n",
            ),
            (
                Some("{\"baz\":\"kept\",\"unused\":\"a\"}"),
                "{\n  \"baz\": \"kept\",\n  \"unused\": \"a\"\n}\n".to_string(),
                "foo(bar.kept);\n",
            ),
            (
                Some("{\"z_\":\"α\",\"a_\":false}"),
                format!("{{\n  \"z_\": \"{alpha}\",\n  \"a_\": false,\n  \"baz\": \"a\"\n}}\n"),
                "foo(bar.a);\n",
            ),
            (
                Some("{\"a_\":false,\"z_\":\"α\"}"),
                format!("{{\n  \"a_\": false,\n  \"baz\": \"a\",\n  \"z_\": \"{alpha}\"\n}}\n"),
                "foo(bar.a);\n",
            ),
        ];
        for (initial, expected, code) in cases {
            let path = directory.join("cache.json");
            let _ = std::fs::remove_file(&path);
            if let Some(initial) = initial {
                std::fs::write(&path, initial).unwrap();
            }
            let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
                .args([
                    "in.js",
                    "--mangle-props=.",
                    "--mangle-cache=cache.json",
                    if utf8 {
                        "--charset=utf8"
                    } else {
                        "--charset=ascii"
                    },
                ])
                .current_dir(&directory)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            assert_eq!(output.stdout, code.as_bytes());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn cli_errors_do_not_overwrite_property_cache_files() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "esbuild-rs-cli-cache-error-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let input = directory.join("in.js");
    let cache = directory.join("cache.json");
    std::fs::write(&input, "foo(bar.baz)").unwrap();
    for contents in [
        "true",
        "[]",
        "{broken}",
        "{\"baz\":true}",
        "{\"baz\":1}",
        "{\"baz\":null}",
        "{\"baz\":{}}",
        "{\"baz\":[]}",
        "{\"baz\":\"__proto__\"}",
    ] {
        std::fs::write(&cache, contents).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(["in.js", "--mangle-props=.", "--mangle-cache=cache.json"])
            .current_dir(&directory)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("[ERROR]"));
        assert!(stderr.ends_with("1 error\n"));
        assert_eq!(std::fs::read_to_string(&cache).unwrap(), contents);
    }
    std::fs::write(&cache, "{}").unwrap();
    std::fs::write(&input, "const x = ;").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .args(["in.js", "--mangle-cache=cache.json"])
        .current_dir(&directory)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(&cache).unwrap(), "{}");
    std::fs::remove_file(&cache).unwrap();
    std::fs::create_dir(&cache).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .args(["in.js", "--mangle-cache=cache.json"])
        .current_dir(&directory)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Failed to read from mangle cache file \"cache.json\": is a directory")
    );
    std::fs::remove_dir_all(directory).unwrap();
}
