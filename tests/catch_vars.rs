use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const CATCH_VARS: &str = r#"
const assert = require('node:assert/strict');
function initialized() {
  var x = 0, values = [];
  try { throw 1; } catch (x) {
    values.push(x);
    var x = 2;
    values.push(x);
  }
  values.push(x);
  assert.deepEqual(values, [1, 2, 0]);
}
function finalized() {
  var x = 0, values = [];
  try { throw 1; } catch (x) {
    values.push(x);
    var x = 2;
    values.push(x);
  } finally { x = 3; }
  values.push(x);
  assert.deepEqual(values, [1, 2, 3]);
}
function uninitialized() {
  const values = [];
  try { throw 1; } catch (x) {
    values.push(x);
    var x = 2;
    values.push(x);
  }
  values.push(x);
  assert.deepEqual(values, [1, 2, undefined]);
}
function nested() {
  const values = [];
  try { throw 1; } catch (x) {
    values.push(x);
    try { throw 2; } catch (x) {
      values.push(x);
      var x = 3;
      values.push(x);
    }
    values.push(x);
  }
  values.push(x);
  assert.deepEqual(values, [1, 2, 3, 1, undefined]);
}
function unusedCatch() {
  try { throw 1; } catch (x) { var x = 2; }
  assert.equal(x, undefined);
}
function parameter(x) {
  try { throw 1; } catch (x) { var x = 2; }
  return x;
}
function evaluated() {
  try { throw 1; } catch (x) {
    var x = 2;
    assert.equal(eval('x'), 2);
  }
  assert.equal(x, undefined);
}
initialized(); finalized(); uninitialized(); nested(); unusedCatch(); evaluated();
assert.equal(parameter(0), 0);
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for catch variable regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn catch_variable_initializers_preserve_outer_hoisted_bindings() {
    execute(CATCH_VARS.as_bytes());
    for target in [Target::Es2015, Target::Es2022] {
        for minify in [false, true] {
            let result = transform(
                CATCH_VARS,
                TransformOptions {
                    target,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute(&result.code);
            let result = build(BuildOptions {
                bundle: true,
                stdin: Some(BuildStdin {
                    contents: CATCH_VARS.into(),
                    ..BuildStdin::default()
                }),
                format: BuildFormat::CommonJs,
                platform: BuildPlatform::Node,
                target,
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute(&result.output_files[0].contents);
        }
    }
}
