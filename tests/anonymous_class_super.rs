use std::collections::HashMap;
use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const ANONYMOUS_SUPER: &str = r#"
const assert = require('node:assert/strict');
function factory(tag) {
  class Base {
    foo() { return this.tag; }
    key() { return this.tag; }
    get value() { return this.stored; }
    set value(value) { this.stored = value; }
    static foo() { return this.tag; }
    static key() { return this.tag; }
  }
  let Derived = class extends Base {
    tag = tag;
    stored = 1;
    async run() {
      const result = super.foo();
      super.value += 2;
      return [result, super.value];
    }
    async nested() {
      return class {
        [super.key()] = 123;
        method() { return this; }
      };
    }
    field = async () => [this.tag, super.foo()];
    static async run() { return super.foo(); }
    static async nested() { return class { [super.key()] = 456; }; }
  };
  const original = Derived;
  original.tag = tag;
  Derived = class Replacement {};
  return original;
}
(async () => {
  for (const tag of ['first', 'second']) {
    const Derived = factory(tag);
    if (globalThis.checkClassNames) assert.equal(Derived.name, 'Derived');
    const instance = new Derived();
    assert.deepEqual(await instance.run(), [tag, 3]);
    assert.deepEqual(await instance.field(), [tag, tag]);
    const Nested = await instance.nested();
    const nested = new Nested();
    assert.equal(nested[tag], 123);
    assert.equal(nested.method(), nested);
    assert.equal(await Derived.run(), tag);
    assert.equal(new (await Derived.nested())()[tag], 456);
    const receiver = { tag: 'borrowed', stored: 10 };
    assert.deepEqual(await Derived.prototype.run.call(receiver), ['borrowed', 12]);
    assert.equal(await Derived.run.call(receiver), 'borrowed');
    Object.setPrototypeOf(Derived.prototype, {
      foo() { return this.tag + ':changed'; },
      get value() { return this.stored; },
      set value(value) { this.stored = value; },
    });
    assert.deepEqual(await instance.run(), [tag + ':changed', 5]);
    Object.setPrototypeOf(Derived, { foo() { return this.tag + ':changed'; } });
    assert.equal(await Derived.run(), tag + ':changed');
  }
  console.log('ok');
})().catch(error => { console.error(error); process.exitCode = 1; });
"#;

fn execute(code: &[u8], keep_names: bool) {
    let output = Command::new("node")
        .arg("-e")
        .arg(format!(
            "globalThis.checkClassNames = {keep_names};\n{}",
            String::from_utf8_lossy(code)
        ))
        .output()
        .expect("Node.js is required for anonymous class super regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn anonymous_class_expressions_provide_a_home_for_lowered_super() {
    execute(ANONYMOUS_SUPER.as_bytes(), true);
    for (target, supported) in [
        (Target::Es2015, HashMap::new()),
        (Target::Es2017, HashMap::new()),
        (Target::Es2022, HashMap::new()),
        (
            Target::Es2022,
            HashMap::from([
                ("async-await".into(), false),
                ("class-static-field".into(), false),
            ]),
        ),
    ] {
        for minify in [false, true] {
            for keep_names in [false, true] {
                let result = transform(
                    ANONYMOUS_SUPER,
                    TransformOptions {
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
                        contents: ANONYMOUS_SUPER.into(),
                        ..BuildStdin::default()
                    }),
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
