use std::collections::HashMap;
use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const PRIVATE_SUPER: &str = r#"
const assert = require('node:assert/strict');
function factory(tag) {
  class Base {
    foo(suffix) { return this.tag + suffix; }
    get value() { return this.stored; }
    set value(value) { this.stored = value; }
    static foo(suffix) { return this.tag + suffix; }
    static get value() { return this.stored; }
    static set value(value) { this.stored = value; }
  }
  class Derived extends Base {
    tag = tag;
    stored = 1;
    #direct() { return super.foo(':direct'); }
    async #async() { return super.foo(':async'); }
    #arrow() { return async () => () => this.tag + super.foo(':arrow'); }
    #nested() { return () => async () => this.tag + super.foo(':nested'); }
    #field = async () => this.tag + super.foo(':field');
    get #value() { return super.value; }
    set #value(value) { super.value = value; }
    #compound() {
      let calls = 0;
      const key = () => (++calls, 'value');
      const result = super[key()] += 3;
      assert.equal(calls, 1);
      return result;
    }
    async run() {
      assert.equal(this.#direct(), tag + ':direct');
      assert.equal(await this.#async(), tag + ':async');
      assert.equal((await this.#arrow()())(), tag + tag + ':arrow');
      assert.equal(await this.#nested()()(), tag + tag + ':nested');
      assert.equal(await this.#field(), tag + tag + ':field');
      this.#value = 7;
      assert.equal(this.#value, 7);
      assert.equal(this.#compound(), 10);
      assert.equal(this.stored, 10);
    }
    static tag = tag;
    static stored = 2;
    static #staticDirect() { return super.foo(':static'); }
    static async #staticAsync() { return super.foo(':async'); }
    static #staticArrow() { return async () => this.tag + super.foo(':arrow'); }
    static get #staticValue() { return super.value; }
    static set #staticValue(value) { super.value = value; }
    static async run() {
      assert.equal(this.#staticDirect(), tag + ':static');
      assert.equal(await this.#staticAsync(), tag + ':async');
      assert.equal(await this.#staticArrow()(), tag + tag + ':arrow');
      this.#staticValue = 8;
      assert.equal(this.#staticValue, 8);
      assert.equal(this.stored, 8);
    }
  }
  const original = Derived;
  Derived = class Replacement {};
  return original;
}
(async () => {
  for (const tag of ['first', 'second']) {
    const Derived = factory(tag);
    await new Derived().run();
    await Derived.run();
  }
  console.log('ok');
})().catch(error => { console.error(error); process.exitCode = 1; });
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for private method super regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn moved_private_methods_lower_super_with_native_and_lowered_async_syntax() {
    execute(PRIVATE_SUPER.as_bytes());
    for (target, supported) in [
        (Target::Es2015, HashMap::new()),
        (Target::Es2017, HashMap::new()),
        (Target::Es2022, HashMap::new()),
        (
            Target::Es2022,
            HashMap::from([
                ("class-private-method".into(), false),
                ("class-private-accessor".into(), false),
                ("class-private-static-method".into(), false),
                ("class-private-static-accessor".into(), false),
            ]),
        ),
    ] {
        for minify in [false, true] {
            for keep_names in [false, true] {
                let result = transform(
                    PRIVATE_SUPER,
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
                execute(&result.code);
                let result = build(BuildOptions {
                    bundle: true,
                    stdin: Some(BuildStdin {
                        contents: PRIVATE_SUPER.into(),
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
                execute(&result.output_files[0].contents);
            }
        }
    }
}
