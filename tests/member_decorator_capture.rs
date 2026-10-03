use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Loader, Target, TransformOptions, build,
    transform,
};

const MEMBER_DECORATORS: &str = r#"
const assert = require('node:assert/strict');
const events = [];
function method(value) {
  assert.equal(value, 12);
  return (target, key, descriptor) => {
    const Box = typeof target === 'function' ? target : target.constructor;
    assert.equal(Box.msgLength, 12);
    events.push(key);
    return descriptor;
  };
}
function parameter(value) {
  assert.equal(value, 12);
  return (target, key, index) => { assert.equal(index, 0); events.push('parameter'); };
}
class Foo {
  static message = 'Hello world!';
  static msgLength = Foo.message.length;
  @method(Foo.msgLength)
  instance(@parameter(Foo.msgLength) value) { return Foo; }
  @method(Foo.msgLength)
  static staticMethod() { return Foo; }
}
const original = Foo;
Foo = class Replacement {};
assert.equal(original.msgLength, 12);
assert.equal(new original().instance(1), original);
assert.equal(original.staticMethod(), original);
assert.deepEqual(events, ['parameter', 'instance', 'staticMethod']);
console.log('ok');
"#;

const PRIVATE_PARAMETER_DECORATORS: &str = r#"
const assert = require('node:assert/strict');
const events = [];
const method = value => { assert.equal(value, 12); return (target, key) => { events.push(key); }; };
const parameter = value => { assert.equal(value, 12); return (target, key, index) => { events.push(index); }; };
class Foo {
  static #value = 12;
  @method(Foo?.#value)
  instance(@parameter((() => Foo.#value)()) value) { return Foo; }
}
const original = Foo;
Foo = class Replacement {};
assert.equal(new original().instance(1), original);
assert.deepEqual(events, [0, 'instance']);
console.log('ok');
"#;

const CONSTRUCTOR_PARAMETER_DECORATORS: &str = r#"
const assert = require('node:assert/strict');
const events = [];
const parameter = value => (target, key, index) => { events.push([value, key, index]); };
class Foo {
  constructor(@parameter(5) value) { this.value = value; }
}
assert.equal(new Foo(7).value, 7);
assert.deepEqual(events, [[5, undefined, 0]]);
console.log('ok');
"#;

const CONFIG: &str = r#"{"compilerOptions":{"experimentalDecorators":true}}"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for member decorator capture regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn member_decorators_use_the_initialized_original_class() {
    for (source, private) in [
        (MEMBER_DECORATORS, false),
        (PRIVATE_PARAMETER_DECORATORS, true),
        (CONSTRUCTOR_PARAMETER_DECORATORS, false),
    ] {
        let configurations = if private {
            vec![
                (Target::Es2015, std::collections::HashMap::new()),
                (Target::Es2022, std::collections::HashMap::new()),
                (
                    Target::Es2022,
                    std::collections::HashMap::from([("class-private-static-field".into(), false)]),
                ),
            ]
        } else {
            vec![
                (Target::Es2015, std::collections::HashMap::new()),
                (Target::Es2022, std::collections::HashMap::new()),
                (
                    Target::Es2022,
                    std::collections::HashMap::from([("class-static-field".into(), false)]),
                ),
            ]
        };
        for (target, supported) in configurations {
            for minify in [false, true] {
                let result = transform(
                    source,
                    TransformOptions {
                        loader: Loader::Ts,
                        tsconfig_raw: CONFIG.into(),
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
                        contents: source.into(),
                        loader: Loader::Ts,
                        ..BuildStdin::default()
                    }),
                    tsconfig_raw: CONFIG.into(),
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
}
