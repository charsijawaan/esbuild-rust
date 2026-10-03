use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const PRIVATE_PATTERNS: &str = r#"
const assert = require('node:assert/strict');
class Pattern {
  #a; #b; #c; #d; #rest; #last;
  values = [];
  getterCalls = 0;
  set #setter(value) { this.values.push(value); }
  get #pair() { this.getterCalls++; return this.values.at(-1); }
  set #pair(value) { this.values.push(value); }
  get #readonly() { return 1; }
  static #staticValue;
  assign() {
    const input = [{x: 1}, [[1, 2, 3]], {}, {x: 2, y: 3, z: 4}, [4, 5, 6, 7], [{x: [{y: [9]}]}]];
    const result = ([
      {x: this.#a}, [[, this.#b, ,]], {y: this.#c = 3},
      {x: this.x, y: this.y, ...this.#d}, [, , ...this.#rest],
      [{x: [{y: [this.#last]}]}],
    ] = input);
    assert.equal(result, input);
    assert.deepEqual([this.#a, this.#b, this.#c, this.#d, this.#rest, this.#last],
      [1, 2, 3, {z: 4}, [6, 7], 9]);
    [this.#setter, {x: this.#pair = 5}] = [4, {}];
    assert.deepEqual(this.values, [4, 5]);
    assert.equal(this.getterCalls, 0);
    const events = [];
    const iterable = {
      [Symbol.iterator]() {
        events.push('iterator');
        return { next() { events.push('next'); return {value: 10}; },
          return() { events.push('close'); return {}; } };
      }
    };
    const receiver = () => { events.push('target'); return this; };
    [receiver().#a, receiver().#b] = iterable;
    assert.deepEqual(events, ['iterator', 'target', 'next', 'target', 'next', 'close']);
    assert.deepEqual([this.#a, this.#b], [10, 10]);
    events.length = 0;
    assert.throws(() => { [({}).#a] = iterable; }, TypeError);
    assert.deepEqual(events, ['iterator', 'next', 'close']);
    assert.throws(() => { [this.#readonly] = [2]; }, TypeError);
  }
  static assign() { [this.#staticValue] = [11]; return this.#staticValue; }
}
new Pattern().assign();
new Pattern().assign();
assert.equal(Pattern.assign(), 11);
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for private assignment pattern regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn private_assignment_patterns_preserve_iteration_and_setters() {
    execute(PRIVATE_PATTERNS.as_bytes());
    for (target, supported) in [
        (Target::Es2015, std::collections::HashMap::new()),
        (Target::Es2022, std::collections::HashMap::new()),
        (
            Target::Es2022,
            std::collections::HashMap::from([("class-private-field".into(), false)]),
        ),
    ] {
        for minify in [false, true] {
            let result = transform(
                PRIVATE_PATTERNS,
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
                    contents: PRIVATE_PATTERNS.into(),
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
