use std::collections::HashMap;
use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Loader, Target, TransformOptions, build,
    transform,
};

const REPLACEMENT: &str = r#"
const assert = require('node:assert/strict');
function factory(tag) {
  const events = [];
  let original, fromBlock;
  const field = value => (events.push('field'), value);
  const member = (target, key) => {
    events.push('member');
    assert.equal(key, 'read');
    assert.equal(target.constructor.ready, tag);
  };
  const parameter = (target, key, index) => {
    events.push('parameter');
    assert.equal(target.ready, tag);
    assert.equal(key, undefined);
    assert.equal(index, 0);
  };
  const replace = value => {
    events.push('class');
    original = value;
    assert.equal(value.ready, tag);
    assert.equal(value.instance.privateValue(), tag);
    assert.equal(value.instance.capture[0], value);
    if (globalThis.checkClassNames) assert.equal(value.name, 'Replacement');
    return { original: value, tag };
  };
  @replace
  class Replacement {
    #value = tag;
    constructor(@parameter value) { this.parameter = value; }
    capture = [Replacement, () => Replacement];
    @member
    read() { return [Replacement, () => Replacement]; }
    privateValue() { return this.#value; }
    static ready = field(tag);
    static capture = [Replacement, () => Replacement];
    static instance = new Replacement('before');
    static { events.push('block'); fromBlock = [Replacement, () => Replacement]; }
    static read() { return [Replacement, () => Replacement]; }
  }
  assert.deepEqual(events, ['field', 'block', 'member', 'parameter', 'class']);
  assert.equal(Replacement.original, original);
  assert.equal(original.capture[0], original);
  assert.equal(original.capture[1](), Replacement);
  assert.equal(fromBlock[0], original);
  assert.equal(fromBlock[1](), Replacement);
  assert.equal(original.instance.capture[1](), Replacement);
  const instance = new original('after');
  assert.equal(instance.parameter, 'after');
  assert.equal(instance.privateValue(), tag);
  assert.equal(instance.capture[0], Replacement);
  assert.equal(instance.capture[1](), Replacement);
  assert.equal(instance.read()[0], Replacement);
  assert.equal(instance.read()[1](), Replacement);
  assert.equal(original.read()[0], Replacement);
  assert.equal(original.read()[1](), Replacement);
  const later = { later: tag };
  Replacement = later;
  assert.equal(instance.read()[0], later);
  assert.equal(original.capture[1](), later);
  assert.equal(fromBlock[1](), later);
  return original;
}
assert.notEqual(factory('first'), factory('second'));
console.log('ok');
"#;

const CONFIG: &str = r#"{"compilerOptions":{"experimentalDecorators":true}}"#;

fn execute(code: &[u8], keep_names: bool) {
    let output = Command::new("node")
        .arg("-e")
        .arg(format!(
            "globalThis.checkClassNames = {keep_names};\n{}",
            String::from_utf8_lossy(code)
        ))
        .output()
        .expect("Node.js is required for class decorator replacement regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn legacy_class_decorators_run_after_initialization_and_update_class_references() {
    for (target, supported) in [
        (Target::Es2015, HashMap::new()),
        (Target::Es2022, HashMap::new()),
        (
            Target::Es2022,
            HashMap::from([("class-static-field".into(), false)]),
        ),
    ] {
        for minify in [false, true] {
            for keep_names in [false, true] {
                let result = transform(
                    REPLACEMENT,
                    TransformOptions {
                        loader: Loader::Ts,
                        tsconfig_raw: CONFIG.into(),
                        target,
                        supported: supported.clone(),
                        keep_names,
                        minify_identifiers: minify,
                        minify_syntax: minify,
                        minify_whitespace: minify,
                        ..TransformOptions::default()
                    },
                );
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                execute(&result.code, keep_names);
                let result = build(BuildOptions {
                    bundle: true,
                    stdin: Some(BuildStdin {
                        contents: REPLACEMENT.into(),
                        loader: Loader::Ts,
                        ..BuildStdin::default()
                    }),
                    tsconfig_raw: CONFIG.into(),
                    format: BuildFormat::CommonJs,
                    platform: BuildPlatform::Node,
                    target,
                    supported: supported.clone(),
                    keep_names,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    ..BuildOptions::default()
                });
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                execute(&result.output_files[0].contents, keep_names);
            }
        }
    }
}
