use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use esbuild_rs::api::{AbsPaths, BuildFormat, BuildOptions, BuildResult, LogLevel, build};
use esbuild_rs::internal::{
    config,
    linker::{AmbiguousReExport, log_ambiguous_re_export},
    logger::{DeferLogKind, Loc, Log, MsgId, MsgKind, PrettyPaths, Source},
};

const ID: &str = "ambiguous-reexport";
static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
const COLLISION: &[(&str, &str)] = &[
    (
        "entry.js",
        "export * from './a.js';\nexport * from './b.js';\nexport * from './c.js';",
    ),
    ("a.js", "export let a = 1, x = 2"),
    ("b.js", "export let b = 3; export { b as x }"),
    ("c.js", "export let c = 4, x = 5"),
];

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-ambiguous-reexports-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        for (path, source) in files {
            let path = directory.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        Self(std::fs::canonicalize(directory).unwrap())
    }

    fn options(&self) -> BuildOptions {
        BuildOptions {
            entry_points: vec!["entry.js".into()],
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            bundle: true,
            format: BuildFormat::CommonJs,
            ..BuildOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn execute_exports(result: &BuildResult) -> String {
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let code = String::from_utf8_lossy(&result.output_files[0].contents);
    let script = format!(
        "const m={{exports:{{}}}}; new Function('module','exports',{})(m,m.exports); console.log(JSON.stringify(m.exports));",
        serde_json::to_string(code.as_ref()).unwrap()
    );
    let output = Command::new("node").args(["-e", &script]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn ambiguous_reexports_default_to_debug_and_all_api_overrides_preserve_output_boundaries() {
    let fixture = Fixture::new(COLLISION);
    let result = build(fixture.options());
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(execute_exports(&result), "{\"a\":1,\"b\":3,\"c\":4}\n");
    for level in [
        LogLevel::Silent,
        LogLevel::Verbose,
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warning,
        LogLevel::Error,
    ] {
        let mut options = fixture.options();
        options.log_override.insert(ID.into(), level);
        let result = build(options);
        assert_eq!(
            result.warnings.len(),
            usize::from(level == LogLevel::Warning)
        );
        assert_eq!(result.errors.len(), usize::from(level == LogLevel::Error));
        assert_eq!(result.output_files.is_empty(), level == LogLevel::Error);
        if level != LogLevel::Error {
            assert_eq!(execute_exports(&result), "{\"a\":1,\"b\":3,\"c\":4}\n");
        }
        for diagnostic in result.warnings.iter().chain(&result.errors) {
            assert_eq!(diagnostic.id, ID);
            assert_eq!(
                diagnostic.text,
                "Re-export of \"x\" in \"entry.js\" is ambiguous and has been removed"
            );
            assert!(diagnostic.location.is_none());
            assert_eq!(diagnostic.notes.len(), 2);
            for (index, (file, column, text)) in [
                (
                    "a.js",
                    18,
                    "One definition of \"x\" comes from \"a.js\" here:",
                ),
                (
                    "b.js",
                    32,
                    "Another definition of \"x\" comes from \"b.js\" here:",
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let note = &diagnostic.notes[index];
                assert_eq!(note.text, text);
                let location = note.location.as_ref().unwrap();
                assert_eq!(
                    (
                        &*location.file,
                        location.line,
                        location.column,
                        location.length
                    ),
                    (file, 1, column, 1)
                );
                assert_eq!(location.line_text, COLLISION[index + 1].1);
                assert!(location.suggestion.is_empty());
            }
        }
    }
}

#[test]
fn internal_logger_retains_debug_messages_and_filters_them_after_overrides() {
    let source = |file: &str, contents: &str| Source {
        pretty_paths: PrettyPaths {
            abs: format!("/{file}"),
            rel: file.into(),
        },
        contents: Arc::from(contents.as_bytes()),
        ..Source::default()
    };
    let entry = source("entry.js", COLLISION[0].1);
    let first = source("a.js", COLLISION[1].1);
    let second = source("b.js", COLLISION[2].1);
    let issue = AmbiguousReExport {
        alias: "x".into(),
        name_loc: Loc { start: 18 },
        other_name_loc: Loc { start: 32 },
        ..AmbiguousReExport::default()
    };
    for (filter, override_level, count, kind) in [
        (DeferLogKind::All, None, 1, MsgKind::Debug),
        (DeferLogKind::NoVerboseOrDebug, None, 0, MsgKind::Debug),
        (
            DeferLogKind::NoVerboseOrDebug,
            Some(esbuild_rs::internal::logger::LogLevel::Warning),
            1,
            MsgKind::Warning,
        ),
        (
            DeferLogKind::All,
            Some(esbuild_rs::internal::logger::LogLevel::Silent),
            0,
            MsgKind::Debug,
        ),
    ] {
        let log = Log::new_defer(
            filter,
            override_level
                .map(|level| HashMap::from([(MsgId::BundlerAmbiguousReexport, level)]))
                .unwrap_or_default(),
        );
        log_ambiguous_re_export(
            &log,
            &config::Options::default(),
            &entry,
            &first,
            &second,
            &issue,
        );
        let messages = log.done();
        assert_eq!(messages.len(), count);
        if let Some(message) = messages.first() {
            assert_eq!(message.kind, kind);
            assert_eq!(message.id, MsgId::BundlerAmbiguousReexport);
            assert!(message.data.location.is_none());
            assert_eq!(message.notes.len(), 2);
            for (note, column) in message.notes.iter().zip([18, 32]) {
                let location = note.location.as_ref().unwrap();
                assert_eq!(
                    (location.line, location.column, location.length),
                    (1, column, 1)
                );
            }
        }
    }
}

#[test]
fn ambiguous_definition_notes_and_message_text_follow_absolute_log_paths() {
    let fixture = Fixture::new(COLLISION);
    let mut options = fixture.options();
    options.abs_paths = AbsPaths::LOG;
    options.log_override.insert(ID.into(), LogLevel::Warning);
    let result = build(options);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.warnings.len(), 1);
    let warning = &result.warnings[0];
    assert_eq!(
        warning.text,
        format!(
            "Re-export of \"x\" in {:?} is ambiguous and has been removed",
            fixture.0.join("entry.js").to_string_lossy()
        )
    );
    for (note, file) in warning.notes.iter().zip(["a.js", "b.js"]) {
        let path = fixture.0.join(file).to_string_lossy().into_owned();
        assert!(note.text.contains(&format!("{path:?}")));
        assert_eq!(note.location.as_ref().unwrap().file, path);
    }
}

#[test]
fn imported_alias_definition_notes_keep_pinned_go_zero_offset_ranges() {
    let files = [
        (
            "entry.js",
            "export * from './a.js'; export * from './b.js';",
        ),
        (
            "a.js",
            "import { one as local } from './first.js'; export { local as shared };",
        ),
        (
            "b.js",
            "import { two as local } from './second.js'; export { local as shared };",
        ),
        ("first.js", "export const one = 1;"),
        ("second.js", "export const two = 2;"),
    ];
    let fixture = Fixture::new(&files);
    let mut options = fixture.options();
    options.log_override.insert(ID.into(), LogLevel::Warning);
    let result = build(options);
    assert_eq!(execute_exports(&result), "{}\n");
    assert_eq!(result.warnings.len(), 1);
    let warning = &result.warnings[0];
    assert_eq!(
        warning.text,
        "Re-export of \"shared\" in \"entry.js\" is ambiguous and has been removed"
    );
    for (index, note) in warning.notes.iter().enumerate() {
        let location = note.location.as_ref().unwrap();
        assert_eq!(location.file, files[index + 1].0);
        assert_eq!((location.line, location.column, location.length), (1, 0, 6));
        assert_eq!(location.line_text, files[index + 1].1);
    }
}

#[test]
fn shared_bindings_and_explicit_exports_are_not_ambiguous() {
    for (files, expected) in [
        (
            vec![
                (
                    "entry.js",
                    "export * from './a.js'; export * from './b.js';",
                ),
                ("a.js", "export { value as shared } from './definition.js';"),
                (
                    "b.js",
                    "import { value as local } from './definition.js'; export { local as shared };",
                ),
                ("definition.js", "export const value = 7;"),
            ],
            "{\"shared\":7}\n",
        ),
        (
            vec![
                (
                    "entry.js",
                    "export * from './a.js'; export * from './b.js'; export const x = 9;",
                ),
                ("a.js", "export const x = 1;"),
                ("b.js", "export const x = 2;"),
            ],
            "{\"x\":9}\n",
        ),
    ] {
        let fixture = Fixture::new(&files);
        let mut options = fixture.options();
        options.log_override.insert(ID.into(), LogLevel::Error);
        let result = build(options);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(execute_exports(&result), expected);
    }
}

#[test]
fn nested_star_reexports_report_each_filtered_namespace_once() {
    let fixture = Fixture::new(&[
        ("entry.js", "export * from './middle.js';"),
        (
            "middle.js",
            "export * from './a.js'; export * from './b.js';",
        ),
        ("a.js", "export const x = 1;"),
        ("b.js", "export const x = 2;"),
    ]);
    let mut options = fixture.options();
    options.log_override.insert(ID.into(), LogLevel::Warning);
    let result = build(options);
    assert_eq!(execute_exports(&result), "{}\n");
    assert_eq!(result.warnings.len(), 2);
    // Go generates locationless messages concurrently and its stable logger
    // sort does not order two locationless messages. Check both namespaces.
    for file in ["entry.js", "middle.js"] {
        let warning = result
            .warnings
            .iter()
            .find(|warning| {
                warning.text
                    == format!("Re-export of \"x\" in {file:?} is ambiguous and has been removed")
            })
            .expect("filtered namespace diagnostic");
        assert_eq!(
            warning.text,
            format!("Re-export of \"x\" in {file:?} is ambiguous and has been removed")
        );
        assert!(warning.location.is_none());
        assert_eq!(warning.notes.len(), 2);
    }
}

#[test]
fn arbitrary_export_aliases_use_go_quoting_and_original_string_ranges() {
    let fixture = Fixture::new(&[
        (
            "entry.js",
            "export * from './a.js'; export * from './b.js';",
        ),
        ("a.js", "const a=1; export {a as 'q\\n\\x01'};"),
        ("b.js", "const b=2; export {b as 'q\\n\\x01'};"),
    ]);
    let mut options = fixture.options();
    options.log_override.insert(ID.into(), LogLevel::Warning);
    let result = build(options);
    assert_eq!(execute_exports(&result), "{}\n");
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(
        result.warnings[0].text,
        "Re-export of \"q\\n\\x01\" in \"entry.js\" is ambiguous and has been removed"
    );
    for note in &result.warnings[0].notes {
        assert!(note.text.contains("\"q\\n\\x01\""));
        let location = note.location.as_ref().unwrap();
        assert_eq!(
            (location.line, location.column, location.length),
            (1, 24, 9)
        );
    }
}

#[test]
fn unicode_alias_quoting_matches_go_print_categories() {
    let alias = "a\u{0301}\u{200c}\u{00a0}\u{feff}\u{e0001}";
    let first = format!("const a=1; export {{a as '{alias}'}};");
    let second = format!("const b=2; export {{b as '{alias}'}};");
    let fixture = Fixture::new(&[
        (
            "entry.js",
            "export * from './a.js'; export * from './b.js';",
        ),
        ("a.js", &first),
        ("b.js", &second),
    ]);
    let mut options = fixture.options();
    options.log_override.insert(ID.into(), LogLevel::Warning);
    let result = build(options);
    assert_eq!(execute_exports(&result), "{}\n");
    assert_eq!(result.warnings.len(), 1);
    let quoted_alias = "\"a\u{0301}\\u200c\\u00a0\\ufeff\\U000e0001\"";
    assert_eq!(
        result.warnings[0].text,
        format!("Re-export of {quoted_alias} in \"entry.js\" is ambiguous and has been removed")
    );
    for note in &result.warnings[0].notes {
        assert!(note.text.contains(quoted_alias));
        let location = note.location.as_ref().unwrap();
        assert_eq!(
            (location.line, location.column, location.length),
            (1, 24, alias.len() + 2)
        );
    }
}
