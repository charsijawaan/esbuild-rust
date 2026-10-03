use std::io::Write;
use std::process::{Command, Stdio};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

fn execute_node(code: &[u8]) {
    let mut child = Command::new("node")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node.js is required for optional-chain executable regressions");
    child.stdin.take().unwrap().write_all(code).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\nGenerated code:\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code),
    );
    assert_eq!(output.stdout, b"ok\n");
}

fn check_lowering(source: &str) {
    execute_node(source.as_bytes());
    for target in [Target::Es2015, Target::Es2020] {
        for minify in [false, true] {
            let supported = std::collections::HashMap::from([("optional-chain".into(), false)]);
            let result = transform(
                source,
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
            assert!(!result.code.windows(2).any(|bytes| bytes == b"?."));
            execute_node(&result.code);
            let result = build(BuildOptions {
                bundle: true,
                format: BuildFormat::CommonJs,
                platform: BuildPlatform::Node,
                stdin: Some(BuildStdin {
                    contents: source.into(),
                    ..BuildStdin::default()
                }),
                target,
                supported,
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            let code = &result.output_files[0].contents;
            assert!(!code.windows(2).any(|bytes| bytes == b"?."));
            execute_node(code);
        }
    }
}

#[test]
fn nested_optional_segments_preserve_parameter_bindings_and_receivers() {
    check_lowering(
        r"
const assert = require('node:assert/strict');
const object = {value: 13, method() { assert.equal(this, object); return this.value; }};
const arrow = (value = object?.method?.()) => value;
const expression = function(value = object?.method?.()) { return value; };
function declaration(value = object?.method?.()) { return value; }
const keyArrow = ({[object?.method?.()]: value}) => value;
const keyExpression = function({[object?.method?.()]: value}) { return value; };
function keyDeclaration({[object?.method?.()]: value}) { return value; }
const bindingArrow = ({value = object?.method?.()}) => value;
const bindingExpression = function({value = object?.method?.()}) { return value; };
function bindingDeclaration({value = object?.method?.()}) { return value; }
assert.deepEqual([arrow(), expression(), declaration(),
  keyArrow({13: 13}), keyExpression({13: 13}), keyDeclaration({13: 13}),
  bindingArrow({}), bindingExpression({}), bindingDeclaration({})], Array(9).fill(13));
let base = {child: object}, events = [];
const get = () => (events.push('base'), base);
const key = () => (events.push('key'), 'method');
const argument = () => (events.push('argument'), 1);
assert.equal(get()?.child?.[key()]?.(argument()), 13);
assert.deepEqual(events, ['base', 'key', 'argument']);
events = []; base = null;
assert.equal(get()?.child?.[key()]?.(argument()), undefined);
assert.deepEqual(events, ['base']);
events = []; base = {child: null};
assert.equal(get()?.child?.[key()]?.(argument()), undefined);
assert.deepEqual(events, ['base']);
events = []; base = {child: {method: null}};
assert.equal(get()?.child?.[key()]?.(argument()), undefined);
assert.deepEqual(events, ['base', 'key']);
const first = value => second;
const second = value => third;
const third = value => value + 1;
assert.equal(first?.(1)?.(2)?.(3), 4);
assert.equal(base?.missing?.deep?.value, undefined);
console.log('ok');
",
    );
}

#[test]
fn nested_parameter_captures_stay_in_scope_during_reentrant_calls() {
    // Native execution is the reference here: the pinned Go lowerer puts some
    // starting-value captures inside the inner chain's arrow, outside the scope
    // of the outer optional call that also needs their receiver.
    check_lowering(
        r"
const assert = require('node:assert/strict');
const first = {value: 11, method(value) { assert.equal(this, first); return this.value + value; }};
const second = {value: 22, method(value) { assert.equal(this, second); return this.value + value; }};
let current = first, reads = 0, argumentsRead = 0, reenter = true;
const get = () => (++reads, current);
const argument = () => {
  ++argumentsRead;
  if (reenter) {
    reenter = false; current = second;
    assert.equal(run(), 25);
    current = first;
  }
  return 3;
};
function run(value = get()?.method?.(argument())) { return value; }
assert.equal(run(), 14);
assert.equal(reads, 2);
assert.equal(argumentsRead, 2);
const outer = {nested: first};
function nested(value = outer?.nested?.method?.(3)) { return value; }
assert.equal(nested(), 14);
function lexical(value = this?.nested?.method?.(arguments[1])) { return value; }
assert.equal(lexical.call(outer, undefined, 3), 14);
current = null;
assert.equal(run(), undefined);
assert.equal(reads, 3);
assert.equal(argumentsRead, 2);
console.log('ok');
",
    );
}
