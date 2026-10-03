use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, TransformOptions, build, transform,
};

const SWITCH_SCOPE: &str = r#"
const assert = require('node:assert/strict');
globalThis.deadConst = 'global';
function skipped() {
  switch (1) {
    case 0: const deadConst = 0;
    case 1: case 2: return deadConst;
  }
}
function before() {
  switch (0) {
    case 0: return laterConst;
    case 1: const laterConst = 0;
  }
}
function defaultCase() {
  switch (1) {
    default: const defaultConst = 0;
    case 1: return defaultConst;
  }
}
function reached(value) {
  switch (value) {
    case 0: const reachedConst = 7;
    case 1: return reachedConst;
  }
}
for (const fn of [skipped, before, defaultCase]) assert.throws(fn, ReferenceError);
assert.equal(reached(0), 7);
assert.throws(() => reached(1), ReferenceError);
delete globalThis.deadConst;
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for switch scope regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn switch_case_constants_preserve_the_shared_scope_and_tdz() {
    execute(SWITCH_SCOPE.as_bytes());
    for minify in [false, true] {
        let result = transform(
            SWITCH_SCOPE,
            TransformOptions {
                minify_syntax: true,
                minify_identifiers: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        execute(&result.code);
        let result = build(BuildOptions {
            bundle: true,
            stdin: Some(BuildStdin {
                contents: SWITCH_SCOPE.into(),
                ..BuildStdin::default()
            }),
            format: BuildFormat::CommonJs,
            platform: BuildPlatform::Node,
            minify_syntax: true,
            minify_identifiers: minify,
            minify_whitespace: minify,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        execute(&result.output_files[0].contents);
    }
}
