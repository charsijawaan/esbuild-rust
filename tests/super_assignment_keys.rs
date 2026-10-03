use std::collections::HashMap;
use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const SUPER_ASSIGNMENTS: &str = r#"
const assert = require('node:assert/strict');
const events = [];
function key() { events.push('key'); return 'value'; }
function rhs(value) { events.push('rhs'); return value; }
function check(value, expected) {
  assert.equal(value, expected);
  assert.deepEqual(events.splice(0), ['key', 'get', 'rhs', 'set']);
}
class Base {
  get value() { events.push('get'); return this.stored; }
  set value(value) { events.push('set'); this.stored = value; }
  static get value() { events.push('get'); return this.stored; }
  static set value(value) { events.push('set'); this.stored = value; }
}
class Derived extends Base {
  get value() { throw new Error('wrong getter'); }
  set value(_) { throw new Error('wrong setter'); }
  async run() {
    this.stored = 6;
    check(super[key()] += rhs(2), 8);
    check(super[key()] -= rhs(3), 5);
    check(super[key()] *= rhs(2), 10);
    check(super[key()] /= rhs(2), 5);
    check(super[key()] %= rhs(3), 2);
    check(super[key()] **= rhs(3), 8);
    check(super[key()] <<= rhs(1), 16);
    check(super[key()] >>= rhs(1), 8);
    check(super[key()] >>>= rhs(1), 4);
    check(super[key()] |= rhs(1), 5);
    check(super[key()] &= rhs(3), 1);
    check(super[key()] ^= rhs(3), 2);
    this.stored = 3n;
    check(super[key()] += rhs(2n), 5n);
    check(super[key()] **= rhs(2n), 25n);
    assert.throws(() => super[key()] += (() => { throw new Error('rhs'); })(), /rhs/);
    assert.deepEqual(events.splice(0), ['key', 'get']);
    assert.equal(this.stored, 25n);
  }
  static get value() { throw new Error('wrong static getter'); }
  static set value(_) { throw new Error('wrong static setter'); }
  static run = async () => {
    this.stored = 4;
    check(super[key()] += rhs(3), 7);
    check(super[key()] **= rhs(2), 49);
    assert.equal(this.stored, 49);
  };
}
(async () => {
  await new Derived().run();
  await Derived.run();
  console.log('ok');
})().catch(error => { console.error(error); process.exitCode = 1; });
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for super assignment regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn lowered_super_assignments_evaluate_computed_keys_once() {
    execute(SUPER_ASSIGNMENTS.as_bytes());
    for (target, supported) in [
        (
            Target::Es2015,
            HashMap::from([("exponent-operator".into(), true)]),
        ),
        (
            Target::Es2017,
            HashMap::from([("async-await".into(), false)]),
        ),
        (
            Target::Es2022,
            HashMap::from([
                ("async-await".into(), false),
                ("class-static-field".into(), false),
            ]),
        ),
    ] {
        for minify in [false, true] {
            let result = transform(
                SUPER_ASSIGNMENTS,
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
                    contents: SUPER_ASSIGNMENTS.into(),
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
