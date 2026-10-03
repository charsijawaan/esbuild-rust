use esbuild_rs::api::{
    BuildFormat, BuildJsx, BuildOptions, Loader, LogLevel, Message, TransformOptions, build,
    transform,
};

const ID: &str = "call-import-namespace";
const TS_NOTE: &str = "Make sure to enable TypeScript's \"esModuleInterop\" setting so that TypeScript's type checker generates an error when you try to do this. You can read more about this setting here: https://www.typescriptlang.org/tsconfig#esModuleInterop";

fn check_warning(message: &Message, name: &str, verb: &str, context: &str, noun: &str) {
    assert_eq!(message.id, ID);
    assert_eq!(
        message.text,
        format!(
            "{verb} {name:?}{context} will crash at run-time because it's an import namespace object, not a {noun}"
        )
    );
    assert_eq!(
        message.notes[0].text,
        format!("Consider changing {name:?} to a default import instead:")
    );
    assert_eq!(message.notes[0].location.as_ref().unwrap().suggestion, name);
}

#[test]
fn namespace_calls_deduplicate_by_binding_and_kind_and_ignore_other_imports() {
    let source = "import * as ns from 'pkg';\nns(); ns?.(); new ns(); new ns; ns.method(); ns['method'](); ns`tag`;\nfunction shadow(ns) { ns(); new ns(); }\nimport other from 'pkg'; import {member} from 'pkg'; other(); new other; member(); new member;";
    for minify in [false, true] {
        let result = transform(
            source,
            TransformOptions {
                sourcefile: "input.js".into(),
                format: BuildFormat::EsModule,
                minify_syntax: minify,
                minify_identifiers: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), 2, "{:?}", result.warnings);
        for (message, verb, noun, column) in [
            (&result.warnings[0], "Calling", "function", 0),
            (&result.warnings[1], "Constructing", "constructor", 18),
        ] {
            check_warning(message, "ns", verb, "", noun);
            let location = message.location.as_ref().unwrap();
            assert_eq!(
                (
                    &*location.file,
                    location.line,
                    location.column,
                    location.length
                ),
                ("input.js", 2, column, 2)
            );
            assert_eq!(location.line_text, source.lines().nth(1).unwrap());
            assert_eq!(message.notes.len(), 1);
            let note = message.notes[0].location.as_ref().unwrap();
            assert_eq!((note.line, note.column, note.length), (1, 7, 7));
            assert_eq!(note.line_text, source.lines().next().unwrap());
        }
    }
}

#[test]
fn namespace_warning_deduplication_distinguishes_two_bindings_from_the_same_module() {
    let source = "import * as ns from 'pkg'; import * as other from 'pkg';\nns(); ns(); other(); other(); new ns; new ns; new other; new other;";
    let result = transform(
        source,
        TransformOptions {
            format: BuildFormat::EsModule,
            ..TransformOptions::default()
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.warnings.len(), 4, "{:?}", result.warnings);
    for (warning, (name, verb, noun)) in result.warnings.iter().zip([
        ("ns", "Calling", "function"),
        ("other", "Calling", "function"),
        ("ns", "Constructing", "constructor"),
        ("other", "Constructing", "constructor"),
    ]) {
        check_warning(warning, name, verb, "", noun);
        let target = if verb == "Constructing" {
            format!("new {name}")
        } else {
            name.into()
        };
        let column = source.lines().nth(1).unwrap().find(&target).unwrap()
            + if verb == "Constructing" { 4 } else { 0 };
        let location = warning.location.as_ref().unwrap();
        assert_eq!(
            (location.line, location.column, location.length),
            (2, column, name.len())
        );
    }
}

#[test]
fn namespace_warnings_require_format_conversion_and_follow_minified_targets() {
    let source = "import * as ns from 'pkg'; ns(); new ns();";
    for format in [
        BuildFormat::Default,
        BuildFormat::EsModule,
        BuildFormat::CommonJs,
        BuildFormat::Iife,
    ] {
        let result = transform(
            source,
            TransformOptions {
                format,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(
            result.warnings.len(),
            if format == BuildFormat::Default { 0 } else { 2 }
        );
    }
    for minify_syntax in [false, true] {
        let result = transform(
            "import * as ns from 'pkg'; (0, ns)();",
            TransformOptions {
                format: BuildFormat::EsModule,
                minify_syntax,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), usize::from(minify_syntax));
        if minify_syntax {
            let location = result.warnings[0].location.as_ref().unwrap();
            assert_eq!((location.column, location.length), (31, 2));
        }
    }
    let result = transform(
        "// @jsx A\nimport * as A from 'pkg';\nA(); new A(); <A/>; <div/>;",
        TransformOptions {
            loader: Loader::Tsx,
            ..TransformOptions::default()
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
}

#[test]
fn jsx_components_and_typescript_notes_preserve_locations_and_usage_kinds() {
    let source = "import * as A from 'pkg';\n<A/>; <A/>; <A.Member/>; A(); new A();";
    for loader in [Loader::Jsx, Loader::Tsx] {
        for jsx in [BuildJsx::Transform, BuildJsx::Preserve, BuildJsx::Automatic] {
            let result = transform(
                source,
                TransformOptions {
                    loader,
                    jsx,
                    sourcefile: "input.tsx".into(),
                    format: BuildFormat::EsModule,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            assert_eq!(result.warnings.len(), 3, "{:?}", result.warnings);
            for (index, (verb, context, noun, column)) in [
                ("Using", " in a JSX expression", "component", 1),
                ("Calling", "", "function", 25),
                ("Constructing", "", "constructor", 34),
            ]
            .into_iter()
            .enumerate()
            {
                let warning = &result.warnings[index];
                check_warning(warning, "A", verb, context, noun);
                let location = warning.location.as_ref().unwrap();
                assert_eq!(
                    (location.line, location.column, location.length),
                    (2, column, 1)
                );
                let note = warning.notes[0].location.as_ref().unwrap();
                assert_eq!((note.line, note.column, note.length), (1, 7, 6));
                assert_eq!(
                    warning.notes.len(),
                    if loader == Loader::Tsx { 2 } else { 1 }
                );
                if loader == Loader::Tsx {
                    assert_eq!(warning.notes[1].text, TS_NOTE);
                    assert!(warning.notes[1].location.is_none());
                }
            }
        }
    }
}

#[test]
fn jsx_factory_warning_uses_the_jsx_location_and_fragment_imports_are_not_calls() {
    let source = "// @jsx factory\nimport * as factory from 'pkg';\n<div/>; <span/>;";
    let result = transform(
        source,
        TransformOptions {
            loader: Loader::Jsx,
            format: BuildFormat::EsModule,
            ..TransformOptions::default()
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    check_warning(&result.warnings[0], "factory", "Calling", "", "function");
    let location = result.warnings[0].location.as_ref().unwrap();
    assert_eq!((location.line, location.column, location.length), (3, 0, 0));
    let note = result.warnings[0].notes[0].location.as_ref().unwrap();
    assert_eq!((note.line, note.column, note.length), (2, 7, 12));
    for source in [
        source,
        "// @jsxFragment A\nimport * as A from 'pkg';\n<>text</>;",
    ] {
        let result = transform(
            source,
            TransformOptions {
                loader: Loader::Jsx,
                jsx: BuildJsx::Preserve,
                format: BuildFormat::EsModule,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }
    let result = transform(
        "// @jsxFragment A\nimport * as A from 'pkg';\n<>text</>;",
        TransformOptions {
            loader: Loader::Jsx,
            format: BuildFormat::EsModule,
            ..TransformOptions::default()
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
}

#[test]
fn escaped_import_names_use_source_ranges_and_decoded_default_import_suggestions() {
    let source = "import * /* comment */ as nam\\u0065 from 'pkg';\nnam\\u0065();";
    let result = transform(
        source,
        TransformOptions {
            format: BuildFormat::CommonJs,
            ..TransformOptions::default()
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.warnings.len(), 1);
    check_warning(&result.warnings[0], "name", "Calling", "", "function");
    let location = result.warnings[0].location.as_ref().unwrap();
    assert_eq!((location.line, location.column, location.length), (2, 0, 9));
    // Go's RangeOfOperatorBefore deliberately finds the last '*' before the name,
    // including the '*' in this comment, instead of storing the import token range.
    let note = result.warnings[0].notes[0].location.as_ref().unwrap();
    assert_eq!((note.line, note.column, note.length), (1, 20, 15));
}

#[test]
fn unicode_identifier_names_use_go_quoting_and_literal_suggestions() {
    let name = "a\u{0301}\u{200c}\u{200d}";
    let result = transform(
        format!("import * as {name} from 'pkg';\n{name}();"),
        TransformOptions {
            format: BuildFormat::EsModule,
            ..TransformOptions::default()
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.warnings.len(), 1);
    let warning = &result.warnings[0];
    assert_eq!(warning.id, ID);
    assert_eq!(
        warning.text,
        "Calling \"a\u{0301}\\u200c\\u200d\" will crash at run-time because it's an import namespace object, not a function"
    );
    assert_eq!(
        warning.notes[0].text,
        "Consider changing \"a\u{0301}\\u200c\\u200d\" to a default import instead:"
    );
    let location = warning.location.as_ref().unwrap();
    assert_eq!(
        (location.line, location.column, location.length),
        (2, 0, name.len())
    );
    let note = warning.notes[0].location.as_ref().unwrap();
    assert_eq!(note.suggestion, name);
    assert_eq!(
        (note.line, note.column, note.length),
        (1, 7, 5 + name.len())
    );
}

#[test]
fn dependency_files_keep_namespace_warnings_and_log_overrides_apply_to_builds() {
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = std::env::temp_dir().join(format!(
        "esbuild-rs-namespace-warnings-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let fixture = Fixture(directory);
    std::fs::create_dir_all(fixture.0.join("node_modules/pkg")).unwrap();
    std::fs::write(
        fixture.0.join("entry.js"),
        "import './node_modules/pkg/index.js';",
    )
    .unwrap();
    std::fs::write(
        fixture.0.join("node_modules/pkg/index.js"),
        "import * as ns from 'dependency'; ns(); new ns();",
    )
    .unwrap();
    for level in [
        LogLevel::Warning,
        LogLevel::Silent,
        LogLevel::Error,
        LogLevel::Info,
        LogLevel::Debug,
        LogLevel::Verbose,
    ] {
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into()],
            abs_working_dir: fixture.0.to_string_lossy().into_owned(),
            bundle: true,
            format: BuildFormat::EsModule,
            external: vec!["dependency".into()],
            log_override: std::collections::HashMap::from([(ID.into(), level)]),
            ..BuildOptions::default()
        });
        assert_eq!(
            result.warnings.len(),
            if level == LogLevel::Warning { 2 } else { 0 }
        );
        assert_eq!(
            result.errors.len(),
            if level == LogLevel::Error { 2 } else { 0 }
        );
        assert_eq!(result.output_files.is_empty(), level == LogLevel::Error);
        for message in result.warnings.iter().chain(&result.errors) {
            assert_eq!(message.id, ID);
            assert_eq!(
                message.location.as_ref().unwrap().file,
                "node_modules/pkg/index.js"
            );
            assert_eq!(message.notes[0].location.as_ref().unwrap().suggestion, "ns");
        }
    }
    for (level, errors, warnings) in [
        (LogLevel::Silent, 0, 0),
        (LogLevel::Error, 2, 0),
        (LogLevel::Warning, 0, 2),
        (LogLevel::Info, 0, 0),
        (LogLevel::Debug, 0, 0),
        (LogLevel::Verbose, 0, 0),
    ] {
        let result = transform(
            "import * as ns from 'pkg'; ns(); new ns();",
            TransformOptions {
                format: BuildFormat::EsModule,
                log_override: std::collections::HashMap::from([(ID.into(), level)]),
                ..TransformOptions::default()
            },
        );
        assert_eq!(result.errors.len(), errors);
        assert_eq!(result.warnings.len(), warnings);
    }
}
