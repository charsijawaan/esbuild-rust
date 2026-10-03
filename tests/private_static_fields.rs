use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const PRIVATE_STATIC_INITIALIZATION: &str = r#"
const assert = require('node:assert/strict');
const events = [];
function factory() {
  class Box {
    #value = 1;
    #method() { return this.#value; }
    get #accessor() { return this.#value; }
    static #staticValue = 2;
    static #staticMethod() { return this.#staticValue; }
    static get #staticAccessor() { return this.#staticValue; }
    static instance = new Box();
    static result = this.instance.#method() + this.#staticMethod() + this.instance.#accessor + this.#staticAccessor;
    static later = () => this.#staticMethod() + this.instance.#method();
    static { events.push(this.result); }
    read() { return this.#method() + this.#accessor; }
    static read() { return this.#staticMethod() + this.#staticAccessor; }
  }
  return Box;
}
const First = factory();
const Second = factory();
for (const Box of [First, Second]) {
  assert.equal(Box.result, 6);
  assert.equal(Box.later(), 3);
  assert.equal(Box.instance.read(), 2);
  assert.equal(Box.read(), 4);
}
assert.deepEqual(events, [6, 6]);
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for partial private member lowering regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn moved_static_initializers_lower_all_private_members() {
    execute(PRIVATE_STATIC_INITIALIZATION.as_bytes());
    for feature in [
        "class-static-field",
        "class-private-method",
        "class-private-static-method",
        "class-private-field",
        "class-private-static-field",
        "class-static-blocks",
    ] {
        for minify in [false, true] {
            let supported = std::collections::HashMap::from([(feature.into(), false)]);
            let result = transform(
                PRIVATE_STATIC_INITIALIZATION,
                TransformOptions {
                    target: Target::Es2022,
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
                    contents: PRIVATE_STATIC_INITIALIZATION.into(),
                    ..BuildStdin::default()
                }),
                format: BuildFormat::CommonJs,
                platform: BuildPlatform::Node,
                target: Target::Es2022,
                supported,
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
