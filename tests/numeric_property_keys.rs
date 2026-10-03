use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, TransformOptions, build, transform,
};

const NUMERIC_KEYS: &str = r#"
const assert = require('node:assert/strict');
const object = {
  [-0]: 0, [-1]: 1, [NaN]: 2, [Infinity]: 3, [-Infinity]: 4,
  [1e5]: 5, [-1e5]: 6, [1e100]: 7, [-1e100]: 8,
  [0xFFFF_FFFF_FFFF]: 9, [-0xFFFF_FFFF_FFFF]: 10,
};
class Fields {
  static [-0] = 0; static [-1] = 1; static [NaN] = 2;
  static [Infinity] = 3; static [-Infinity] = 4;
  static [1e5] = 5; static [-1e5] = 6;
  static [1e100] = 7; static [-1e100] = 8;
  static [0xFFFF_FFFF_FFFF] = 9; static [-0xFFFF_FFFF_FFFF] = 10;
}
class Methods {
  [-0]() { return 0; } [-1]() { return 1; } [NaN]() { return 2; }
  [Infinity]() { return 3; } [-Infinity]() { return 4; }
  [1e5]() { return 5; } [-1e5]() { return 6; }
  [1e100]() { return 7; } [-1e100]() { return 8; }
  [0xFFFF_FFFF_FFFF]() { return 9; } [-0xFFFF_FFFF_FFFF]() { return 10; }
}
const keys = ['0', '-1', 'NaN', 'Infinity', '-Infinity', '100000', '-100000',
  '1e+100', '-1e+100', '281474976710655', '-281474976710655'];
assert.deepEqual(Object.keys(object).sort(), keys.slice().sort());
assert.deepEqual(Object.keys(Fields).sort(), keys.slice().sort());
for (const [value, key] of keys.entries()) {
  assert.equal(object[key], value);
  assert.equal(Fields[key], value);
  assert.equal(new Methods()[key](), value);
  assert.equal(new Methods()[key].name, key);
}
const { [-1]: negative, [-Infinity]: infinity } = object;
assert.equal(negative, 1);
assert.equal(infinity, 4);
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for numeric property key regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn folded_numeric_keys_keep_valid_property_syntax() {
    execute(NUMERIC_KEYS.as_bytes());
    for minify in [false, true] {
        let result = transform(
            NUMERIC_KEYS,
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
                contents: NUMERIC_KEYS.into(),
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
