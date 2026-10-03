use std::{
    collections::HashMap,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use esbuild_rs::api::{BuildFormat, BuildOptions, BuildPlatform, BuildStdin, build, context};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "esbuild-rs-injection-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }

    fn options(&self) -> BuildOptions {
        BuildOptions {
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            inject: vec!["inject.js".into()],
            bundle: true,
            platform: BuildPlatform::Node,
            format: BuildFormat::EsModule,
            ..BuildOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(fixture: &Fixture, source: &str, format: BuildFormat) {
    let mut command = Command::new("node");
    command.current_dir(&fixture.0);
    if format == BuildFormat::EsModule {
        command.arg("--input-type=module");
    }
    let output = command.args(["-e", source]).output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{source}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn injection_preserves_shadowing_dotted_names_defines_and_nonbundled_imports() {
    let fixture = Fixture::new();
    fixture.write(
        "inject.js",
        r#"
export let value = 7;
export function increment() { value++; }
const fn = function() { 'use strict'; return this === undefined; };
const meta = { id: 1 }, metaFoo = { id: 2 }, deep = 3;
export { fn as 'root.fn', meta as 'import.meta', metaFoo as 'import.meta.foo', deep as 'import.meta.foo.bar' };
export let second = 9;
export const both = 'injected';
export { second as 'seco.nd', both as 'bo.th' };
"#,
    );
    let cases = [
        (
            "assert.equal(value, 7); increment(); assert.equal(value, 8); function f(value) { return value; } assert.equal(f(20), 20);",
            true,
            BuildFormat::EsModule,
            HashMap::new(),
        ),
        (
            "globalThis.root = { fn: () => 88 }; assert.equal(root.fn(), true); assert.equal(root['fn'](), EXPECTED); function f(root) { return root.fn(); } assert.equal(f({ fn: () => 42 }), 42); assert.equal(import.meta.id, 1); assert.equal(import.meta.foo.id, 2); assert.equal(import.meta.foo.bar, 3);",
            true,
            BuildFormat::EsModule,
            HashMap::new(),
        ),
        (
            "assert.equal(first, 9); assert.equal(both, 'defined'); assert.equal(fir.st, 9); assert.equal(bo.th, 'defined-dot');",
            true,
            BuildFormat::EsModule,
            HashMap::from([
                ("first".into(), "second".into()),
                ("both".into(), "\"defined\"".into()),
                ("fir.st".into(), "seco.nd".into()),
                ("bo.th".into(), "\"defined-dot\"".into()),
            ]),
        ),
        (
            "assert.equal(value, 7); increment(); assert.equal(value, 8);",
            false,
            BuildFormat::EsModule,
            HashMap::new(),
        ),
        (
            "const value = 42; assert.equal(value, 42);",
            true,
            BuildFormat::EsModule,
            HashMap::new(),
        ),
        (
            "with ({ root: { fn: () => 42 }, value: 99 }) { assert.equal(root.fn(), 42); assert.equal(value, 99); }",
            true,
            BuildFormat::CommonJs,
            HashMap::new(),
        ),
    ];
    for (source, bundle, format, define) in cases {
        for minify in [false, true] {
            let assertions = source.replace("EXPECTED", if minify { "true" } else { "88" });
            let import = if format == BuildFormat::CommonJs {
                "const assert = require('node:assert/strict');"
            } else {
                "import assert from 'node:assert/strict';"
            };
            let result = build(BuildOptions {
                stdin: Some(BuildStdin {
                    contents: format!("{import} {assertions} console.log('ok');"),
                    ..BuildStdin::default()
                }),
                bundle,
                format,
                define: define.clone(),
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..fixture.options()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            run(
                &fixture,
                &String::from_utf8(result.output_files[0].contents.clone()).unwrap(),
                format,
            );
        }
    }
}

#[test]
fn injected_side_effects_run_once_in_order_and_unused_pure_files_are_removed() {
    let fixture = Fixture::new();
    fixture.write("inject.js", "(globalThis.injectionEvents ||= []).push('first'); export const value = 7; export const unused = 'UNUSED_DECLARATION';");
    fixture.write(
        "second.js",
        "globalThis.injectionEvents.push('second'); export const value = 9;",
    );
    fixture.write("node_modules/pure/package.json", r#"{"sideEffects":false}"#);
    fixture.write(
        "node_modules/pure/index.js",
        "globalThis.injectionEvents.push('MUST_NOT_RUN'); export const ignored = 10;",
    );
    fixture.write("dep.js", "export const read = () => value;");
    fixture.write("entry.js", "import assert from 'node:assert/strict'; import { read } from './dep.js'; assert.equal(read(), 7); assert.deepEqual(globalThis.injectionEvents, ['first', 'second']); console.log('ok');");
    for minify in [false, true] {
        let result = build(BuildOptions {
            inject: vec![
                "inject.js".into(),
                "inject.js".into(),
                "second.js".into(),
                "node_modules/pure/index.js".into(),
            ],
            entry_points: vec!["entry.js".into()],
            minify_identifiers: minify,
            minify_syntax: minify,
            minify_whitespace: minify,
            ..fixture.options()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let code = String::from_utf8(result.output_files[0].contents.clone()).unwrap();
        assert!(!code.contains("UNUSED_DECLARATION"), "{code}");
        assert!(!code.contains("MUST_NOT_RUN"), "{code}");
        run(&fixture, &code, BuildFormat::EsModule);
    }
}

#[test]
fn split_injected_bindings_stay_live_through_lazy_commonjs_initialization() {
    let fixture = Fixture::new();
    fixture.write(
        "inject.js",
        "export let value = 7; export function increment() { value++; }",
    );
    fixture.write(
        "entry.js",
        "export const read = () => value; export const change = () => increment();",
    );
    fixture.write(
        "lazy.ts",
        "require('./lazy.ts'); exports.read = () => value;",
    );
    for minify in [false, true] {
        let result = build(BuildOptions {
            entry_points: vec!["entry.js".into(), "lazy.ts".into()],
            outdir: "out".into(),
            splitting: true,
            out_extension: HashMap::from([(".js".into(), ".mjs".into())]),
            minify_identifiers: minify,
            minify_syntax: minify,
            minify_whitespace: minify,
            ..fixture.options()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        std::fs::create_dir_all(fixture.0.join("out")).unwrap();
        for output in result.output_files {
            std::fs::write(output.path, output.contents).unwrap();
        }
        run(
            &fixture,
            "import assert from 'node:assert/strict'; import * as entry from './out/entry.mjs'; import lazy from './out/lazy.mjs'; assert.equal(entry.read(), 7); assert.equal(lazy.read(), 7); entry.change(); assert.equal(entry.read(), 8); assert.equal(lazy.read(), 8); console.log('ok');",
            BuildFormat::EsModule,
        );
    }
}

#[test]
fn context_rebuild_refreshes_injected_exports_and_recovers_from_parse_errors() {
    let fixture = Fixture::new();
    fixture.write("inject.js", "export const value = 7;");
    fixture.write("entry.js", "console.log(value);");
    let context = context(BuildOptions {
        entry_points: vec!["entry.js".into()],
        ..fixture.options()
    })
    .unwrap();
    let first = context.rebuild();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert!(String::from_utf8_lossy(&first.output_files[0].contents).contains("7"));
    fixture.write("inject.js", "export const value = ;");
    assert!(!context.rebuild().errors.is_empty());
    fixture.write("inject.js", "export const value = 9;");
    let result = context.rebuild();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let code = String::from_utf8_lossy(&result.output_files[0].contents);
    let output = Command::new("node")
        .args(["--input-type=module", "-e", &code])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"9\n");
    context.dispose();
}

#[test]
fn assigning_to_injected_names_reports_the_export_location() {
    let fixture = Fixture::new();
    fixture.write(
        "inject.js",
        "export let value = 7; export { value as 'root.value' };",
    );
    for source in ["value = 1;", "root.value++;", "({ value } = other);"] {
        let result = build(BuildOptions {
            stdin: Some(BuildStdin {
                contents: source.into(),
                sourcefile: "entry.js".into(),
                ..BuildStdin::default()
            }),
            ..fixture.options()
        });
        let error = &result.errors[0];
        assert!(
            error
                .text
                .contains("because it's an import from an injected file"),
            "{error:?}"
        );
        assert_eq!(error.location.as_ref().unwrap().file, "entry.js");
        assert_eq!(error.notes.len(), 1);
        assert!(
            error.notes[0]
                .text
                .contains("was exported from \"inject.js\" here:")
        );
        assert_eq!(error.notes[0].location.as_ref().unwrap().file, "inject.js");
        assert!(result.output_files.is_empty());
    }
}
