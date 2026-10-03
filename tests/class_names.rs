use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const CLASS_NAMES: &str = r#"
const assert = require('node:assert/strict');
const events = [];
class Declared {
  static observed = this.name;
  static { events.push(this.name); }
}
const Assigned = class {
  static observed = this.name;
  static { events.push(this.name); }
};
let Later;
Later = class {
  static observed = this.name;
  static { events.push(this.name); }
};
const object = {
  Member: class { static observed = this.name; },
  ['Literal']: class { static observed = this.name; },
};
class Outer {
  Inner = class { static observed = this.name; };
  static Nested = class { static observed = this.name; };
}
const [ArrayClass = class { static observed = this.name; }] = [];
const { ObjectClass = class { static observed = this.name; } } = {};
function factory() {
  return class Local {
    static observed = this.name;
    static #value = this.name;
    static read() { return this.#value; }
  };
}
const Local = factory();
const WithPrivate = class {
  static #value = this.name;
  static read() { return this.#value; }
};
class Custom {
  static name = 'custom';
  static observed = this.name;
}
for (const [value, name] of [
  [Declared, 'Declared'], [Assigned, 'Assigned'], [Later, 'Later'],
  [object.Member, 'Member'], [object.Literal, 'Literal'],
  [new Outer().Inner, 'Inner'], [Outer.Nested, 'Nested'],
  [ArrayClass, 'ArrayClass'], [ObjectClass, 'ObjectClass'], [Local, 'Local'],
  [Custom, 'custom'],
]) {
  assert.equal(value.name, name);
  assert.equal(value.observed, name);
}
assert.deepEqual(events, ['Declared', 'Assigned', 'Later']);
assert.equal(Local.read(), 'Local');
assert.equal(WithPrivate.read(), 'WithPrivate');
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for class name regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn class_names_are_set_before_static_initialization() {
    execute(CLASS_NAMES.as_bytes());
    for (target, lower_blocks) in [
        (Target::Es2015, false),
        (Target::Es2022, false),
        (Target::Es2022, true),
    ] {
        for minify in [false, true] {
            let supported = if lower_blocks {
                std::collections::HashMap::from([("class-static-blocks".into(), false)])
            } else {
                std::collections::HashMap::new()
            };
            let result = transform(
                CLASS_NAMES,
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
                    contents: CLASS_NAMES.into(),
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
