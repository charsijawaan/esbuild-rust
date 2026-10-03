//! Focused regressions for the pinned Go build API's validated settings and
//! independent code/log/metafile paths, including the original typeof warning.

use std::{collections::HashMap, fs, path::PathBuf, time::SystemTime};

use esbuild_rs::api::{
    AbsPaths, BuildFormat, BuildOptions, BuildSourceMap, BuildStdin, LogLevel, Message, build,
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("esbuild-api-validation-paths-{unique}"));
        fs::create_dir_all(directory.join("src")).unwrap();
        // Resolve /tmp once so the assertions use the same absolute paths as
        // Go's real filesystem on macOS as well as on Linux.
        Self(fs::canonicalize(directory).unwrap())
    }

    fn options(&self) -> BuildOptions {
        BuildOptions {
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            ..BuildOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| message.text.as_str())
        .collect()
}

#[test]
fn original_typeof_warning_selects_code_log_and_metafile_paths_independently() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("src/entry.js"), "x = typeof y == \"null\"").unwrap();
    let entry = fixture
        .0
        .join("src/entry.js")
        .to_string_lossy()
        .into_owned();
    let outfile = fixture
        .0
        .join("out/result.js")
        .to_string_lossy()
        .into_owned();
    for abs_paths in [
        AbsPaths::default(),
        AbsPaths::CODE,
        AbsPaths::LOG,
        AbsPaths::METAFILE,
        AbsPaths::CODE | AbsPaths::LOG | AbsPaths::METAFILE,
    ] {
        let result = build(BuildOptions {
            entry_points: vec![entry.clone()],
            outfile: outfile.clone(),
            bundle: true,
            format: BuildFormat::CommonJs,
            metafile: true,
            abs_paths,
            ..fixture.options()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        assert_eq!(result.output_files[0].path, outfile);
        let code_path = if abs_paths.contains(AbsPaths::CODE) {
            &entry
        } else {
            "src/entry.js"
        };
        assert_eq!(
            result.output_files[0].contents,
            format!("// {code_path}\nx = typeof y == \"null\";\n").as_bytes()
        );
        let metadata: serde_json::Value = serde_json::from_str(&result.metafile).unwrap();
        let input_key = if abs_paths.contains(AbsPaths::METAFILE) {
            &entry
        } else {
            "src/entry.js"
        };
        let output_key = if abs_paths.contains(AbsPaths::METAFILE) {
            &outfile
        } else {
            "out/result.js"
        };
        assert_eq!(metadata["inputs"].as_object().unwrap().len(), 1);
        assert_eq!(metadata["outputs"].as_object().unwrap().len(), 1);
        assert_eq!(
            metadata["inputs"][input_key]["imports"],
            serde_json::json!([])
        );
        assert_eq!(metadata["outputs"][output_key]["entryPoint"], input_key);
        assert_eq!(result.warnings.len(), 1);
        let warning = &result.warnings[0];
        assert_eq!(warning.id, "impossible-typeof");
        assert_eq!(
            warning.text,
            "The \"typeof\" operator will never evaluate to \"null\""
        );
        let location = warning.location.as_ref().unwrap();
        assert_eq!(
            location.file,
            if abs_paths.contains(AbsPaths::LOG) {
                &entry
            } else {
                "src/entry.js"
            }
        );
        assert_eq!(location.namespace, "");
        assert_eq!(
            (location.line, location.column, location.length),
            (1, 16, 6)
        );
        assert_eq!(location.line_text, "x = typeof y == \"null\"");
        assert_eq!(warning.notes.len(), 1);
        assert_eq!(
            warning.notes[0].text,
            "The expression \"typeof x\" actually evaluates to \"object\" in JavaScript, not \"null\". You need to use \"x === null\" to test for null."
        );
        assert!(warning.notes[0].location.is_none());
    }
}

#[test]
fn alias_name_normalization_and_empty_substitution_are_native_api_errors() {
    for alias in [
        "./foo",
        "../foo",
        "/foo",
        "C:\\foo",
        ".foo",
        "foo/",
        "@foo/",
        "foo/../bar",
        "",
        "foo//bar",
        "foo/./bar",
        "foo\\bar",
    ] {
        let result = build(BuildOptions {
            bundle: true,
            alias: HashMap::from([(alias.into(), "foo".into())]),
            ..BuildOptions::default()
        });
        assert_eq!(result.errors.len(), 1, "{alias}: {:?}", result.errors);
        assert_eq!(
            result.errors[0].text,
            format!("Invalid alias name: {alias:?}")
        );
        assert!(result.output_files.is_empty());
    }
    let result = build(BuildOptions {
        alias: HashMap::from([("./bad".into(), String::new())]),
        ..BuildOptions::default()
    });
    assert_eq!(texts(&result.errors), ["Invalid alias substitution: \"\""]);
    for alias in ["foo", "foo/bar", "@foo", "@foo/bar", "@foo/bar/baz"] {
        let result = build(BuildOptions {
            stdin: Some(BuildStdin {
                contents: format!("import {alias:?}"),
                ..BuildStdin::default()
            }),
            bundle: true,
            alias: HashMap::from([(alias.into(), "foo".into())]),
            external: vec!["foo".into()],
            format: BuildFormat::EsModule,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{alias}: {:?}", result.errors);
        assert_eq!(
            result.output_files[0].contents,
            b"// <stdin>\nimport \"foo\";\n"
        );
    }
}

#[test]
fn build_only_checks_validated_external_matchers_and_aliases_in_go_order() {
    let invalid = "External path \"a*b*c\" cannot have more than one \"*\" wildcard";
    for (external, expected) in [
        (vec!["a*b*c".into()], vec![invalid]),
        (
            vec!["a*b*c".into(), "pkg".into()],
            vec![invalid, "Cannot use \"external\" without \"bundle\""],
        ),
        (
            vec!["pkg".into()],
            vec!["Cannot use \"external\" without \"bundle\""],
        ),
    ] {
        let result = build(BuildOptions {
            entry_points: vec!["missing.js".into()],
            external,
            ..BuildOptions::default()
        });
        assert_eq!(texts(&result.errors), expected);
        assert!(result.output_files.is_empty());
    }
    let result = build(BuildOptions {
        entry_points: vec!["a.js".into(), "b.js".into()],
        sourcemap: BuildSourceMap::External,
        external: vec!["a*b*c".into(), "pkg".into()],
        alias: HashMap::from([("./bad".into(), "x".into()), ("good".into(), "x".into())]),
        ..BuildOptions::default()
    });
    assert_eq!(
        texts(&result.errors),
        [
            invalid,
            "Invalid alias name: \"./bad\"",
            "Must use \"outdir\" when there are multiple input files",
            "Cannot use \"external\" without \"bundle\"",
            "Cannot use \"alias\" without \"bundle\""
        ]
    );
}

#[test]
fn typeof_warning_covers_equality_orders_and_switch_without_rewriting_code() {
    for minify_syntax in [false, true] {
        for source in [
            "typeof x === 'null'",
            "typeof x !== 'null'",
            "typeof x == 'null'",
            "typeof x != 'null'",
            "'null' === typeof x",
            "'null' !== typeof x",
            "'null' == typeof x",
            "'null' != typeof x",
            "switch (typeof x) { case 'null': }",
            "typeof x === 'invalid'",
        ] {
            let result = build(BuildOptions {
                stdin: Some(BuildStdin {
                    contents: source.into(),
                    ..BuildStdin::default()
                }),
                minify_syntax,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{source}: {:?}", result.errors);
            assert_eq!(result.warnings.len(), 1, "{source}");
            assert_eq!(result.warnings[0].id, "impossible-typeof");
            assert_eq!(
                result.warnings[0].notes.len(),
                usize::from(!source.contains("invalid"))
            );
        }
    }
    for value in [
        "undefined",
        "object",
        "boolean",
        "number",
        "bigint",
        "string",
        "symbol",
        "function",
        "unknown",
    ] {
        let result = build(BuildOptions {
            stdin: Some(BuildStdin {
                contents: format!("typeof x === {value:?}"),
                ..BuildStdin::default()
            }),
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{value}");
    }
    let result = build(BuildOptions {
        stdin: Some(BuildStdin {
            contents: "switch ('null') { case typeof x: }".into(),
            ..BuildStdin::default()
        }),
        ..BuildOptions::default()
    });
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty());
}

#[test]
fn typeof_warning_obeys_log_overrides_and_dependency_suppression() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("node_modules/pkg")).unwrap();
    fs::write(
        fixture.0.join("node_modules/pkg/entry.js"),
        "x = typeof y == 'null'",
    )
    .unwrap();
    for (log_override, warnings, errors) in [
        (HashMap::new(), 0, 0),
        (
            HashMap::from([("impossible-typeof".into(), LogLevel::Warning)]),
            1,
            0,
        ),
        (
            HashMap::from([("impossible-typeof".into(), LogLevel::Error)]),
            0,
            1,
        ),
        (
            HashMap::from([("impossible-typeof".into(), LogLevel::Silent)]),
            0,
            0,
        ),
    ] {
        let result = build(BuildOptions {
            entry_points: vec!["node_modules/pkg/entry.js".into()],
            log_override,
            ..fixture.options()
        });
        assert_eq!(result.errors.len(), errors);
        assert_eq!(result.warnings.len(), warnings);
    }
}
