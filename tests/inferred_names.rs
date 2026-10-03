use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const INFERRED_NAMES: &str = r#"
const assert = require('node:assert/strict');
const object = {};
object.arrow = () => {};
object['function'] = function() {};
object.Class = class {};
object['OtherClass'] = class {};
for (const value of Object.values(object)) assert.equal(value.name, '');
let Arrow, Function, Class;
Arrow = () => {};
Function = function() {};
Class = class {};
assert.equal(Arrow.name, 'Arrow');
assert.equal(Function.name, 'Function');
assert.equal(Class.name, 'Class');
const key = ['computed', 'name'].join('_');
const holder = { key };
const computed = {
  [key]: () => {},
  [holder.key]: function() {},
};
assert.equal(computed[key].name, key);
const { alias: Bound = () => {} } = {};
let Assigned;
({ alias: Assigned = function() {} } = {});
assert.equal(Bound.name, 'Bound');
assert.equal(Assigned.name, 'Assigned');
class Private {
  #method() { return 123; }
  static #staticMethod() { return 456; }
  method = this.#method;
  static method = this.#staticMethod;
}
assert.equal(new Private().method.name, '#method');
assert.equal(Private.method.name, '#staticMethod');
assert.equal(new Private().method(), 123);
assert.equal(Private.method(), 456);
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for inferred name regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn inferred_names_follow_binding_and_property_contexts() {
    execute(INFERRED_NAMES.as_bytes());
    for target in [Target::Es2015, Target::Es2022] {
        for minify in [false, true] {
            let supported = std::collections::HashMap::from([
                ("class-private-method".into(), false),
                ("class-private-static-method".into(), false),
            ]);
            let result = transform(
                INFERRED_NAMES,
                TransformOptions {
                    target,
                    supported: supported.clone(),
                    keep_names: true,
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
                    contents: INFERRED_NAMES.into(),
                    ..BuildStdin::default()
                }),
                format: BuildFormat::CommonJs,
                platform: BuildPlatform::Node,
                target,
                supported,
                keep_names: true,
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
