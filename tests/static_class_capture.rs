use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const STATIC_CAPTURE: &str = r#"
const assert = require('node:assert/strict');
function factory(tag) {
  class Box {
    static tag = tag;
    static direct = this;
    static arrow = () => this;
    static named = () => Box;
    static defaults = (value = this) => value;
    static object = { arrow: () => this, method() { return this; } };
    static nested = () => class {
      [this.tag] = 1;
      static self = this;
      method() { return this; }
    };
    static ordinary = function() { return this; };
    method() { return Box; }
  }
  const original = Box;
  Box = class Replacement {};
  return { original, replacement: Box };
}
const first = factory('first');
const second = factory('second');
assert.notEqual(first.original, second.original);
for (const { original: Box, replacement } of [first, second]) {
  assert.equal(Box.direct, Box);
  assert.equal(Box.arrow.call(replacement), Box);
  assert.equal(Box.named(), Box);
  assert.equal(Box.defaults(), Box);
  assert.equal(Box.object.arrow(), Box);
  assert.equal(Box.object.method(), Box.object);
  assert.equal(Box.ordinary.call(replacement), replacement);
  assert.equal(new Box().method(), Box);
  const Nested = Box.nested();
  const instance = new Nested();
  assert.equal(instance[Box.tag], 1);
  assert.equal(Nested.self, Nested);
  assert.equal(instance.method(), instance);
}
class Base { static value = 3; }
class Derived extends Base {
  static read = () => this.value + super.value;
}
const original = Derived;
Derived = class Replacement {};
assert.equal(original.read(), 6);
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for static class capture regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn moved_static_initializers_capture_the_original_class() {
    execute(STATIC_CAPTURE.as_bytes());
    for (target, supported) in [
        (Target::Es2015, std::collections::HashMap::new()),
        (Target::Es2022, std::collections::HashMap::new()),
        (
            Target::Es2022,
            std::collections::HashMap::from([("class-static-field".into(), false)]),
        ),
    ] {
        for minify in [false, true] {
            let result = transform(
                STATIC_CAPTURE,
                TransformOptions {
                    target,
                    supported: supported.clone(),
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
                    contents: STATIC_CAPTURE.into(),
                    ..BuildStdin::default()
                }),
                format: BuildFormat::CommonJs,
                platform: BuildPlatform::Node,
                target,
                supported: supported.clone(),
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
