use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const ANNEX_B: &str = r#"
const assert = require('node:assert/strict');
let clash = 1;
if (1) function clash() { return clash; }
assert.equal(clash, 1);
var conditional;
if (1) function conditional() { return conditional; }
assert.equal(conditional(), conditional);
label: function labeled() { return labeled; }
assert.equal(labeled(), labeled);
outer: inner: function nested() { return nested; }
assert.equal(nested(), nested);
function factory(flag) {
  var optional;
  if (flag) function optional() { return optional; }
  return optional;
}
assert.equal(factory(false), undefined);
const first = factory(true);
const second = factory(true);
assert.equal(first(), first);
assert.equal(second(), second);
assert.notEqual(first, second);
function labeledFactory() {
  label: function local() { return local; }
  return local;
}
const local = labeledFactory();
assert.equal(local(), local);
if (globalThis.checkFunctionNames) {
  assert.equal(conditional.name, 'conditional');
  assert.equal(labeled.name, 'labeled');
  assert.equal(nested.name, 'nested');
  assert.equal(first.name, 'optional');
  assert.equal(local.name, 'local');
}
console.log('ok');
"#;

fn execute(code: &[u8], format: BuildFormat, keep_names: bool) {
    let esm = format == BuildFormat::EsModule;
    let prefix = if esm {
        "import { createRequire } from 'node:module'; globalThis.require = createRequire(import.meta.url);"
    } else {
        ""
    };
    let output = Command::new("node")
        .arg(if esm {
            "--input-type=module"
        } else {
            "--input-type=commonjs"
        })
        .arg("-e")
        .arg(format!(
            "{prefix} globalThis.checkFunctionNames = {keep_names};\n{}",
            String::from_utf8_lossy(code)
        ))
        .output()
        .expect("Node.js is required for Annex B function regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn sloppy_if_and_labeled_functions_keep_their_hoisted_bindings_in_all_formats() {
    execute(ANNEX_B.as_bytes(), BuildFormat::Default, true);
    for format in [
        BuildFormat::Default,
        BuildFormat::CommonJs,
        BuildFormat::EsModule,
        BuildFormat::Iife,
    ] {
        for minify in [false, true] {
            for keep_names in [false, true] {
                let result = transform(
                    ANNEX_B,
                    TransformOptions {
                        target: Target::Es2015,
                        format,
                        keep_names,
                        minify_identifiers: minify,
                        minify_syntax: minify,
                        minify_whitespace: minify,
                        ..TransformOptions::default()
                    },
                );
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                execute(&result.code, format, keep_names);
                if format != BuildFormat::Default {
                    let result = build(BuildOptions {
                        bundle: true,
                        stdin: Some(BuildStdin {
                            contents: ANNEX_B.into(),
                            ..BuildStdin::default()
                        }),
                        format,
                        platform: BuildPlatform::Node,
                        target: Target::Es2015,
                        keep_names,
                        minify_identifiers: minify,
                        minify_syntax: minify,
                        minify_whitespace: minify,
                        ..BuildOptions::default()
                    });
                    assert!(result.errors.is_empty(), "{:?}", result.errors);
                    execute(&result.output_files[0].contents, format, keep_names);
                }
            }
        }
    }
}

#[test]
fn strict_source_still_rejects_if_and_labeled_functions() {
    for source in [
        "'use strict'; if (1) function f() {}",
        "'use strict'; label: function f() {}",
        "export {}; if (1) function f() {}",
        "export {}; label: function f() {}",
        "class C { method() { label: function f() {} } }",
    ] {
        for format in [
            BuildFormat::Default,
            BuildFormat::CommonJs,
            BuildFormat::EsModule,
        ] {
            let result = transform(
                source,
                TransformOptions {
                    format,
                    ..TransformOptions::default()
                },
            );
            assert_eq!(result.errors.len(), 1, "{source}: {:?}", result.errors);
            assert!(
                result.errors[0]
                    .text
                    .contains("Function declarations inside")
            );
            assert!(!result.errors[0].notes.is_empty());
        }
    }
}
