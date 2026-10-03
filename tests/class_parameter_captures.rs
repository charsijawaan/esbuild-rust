use std::{
    io::Write,
    process::{Command, Stdio},
};

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
        .expect("Node.js is required for class parameter executable regressions");
    child.stdin.take().unwrap().write_all(code).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\nGenerated code:\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

fn check_lowering(source: &str) {
    execute_node(source.as_bytes());
    for (target, supported) in [
        (Target::Es2015, std::collections::HashMap::new()),
        (
            Target::Es2022,
            std::collections::HashMap::from([
                ("class-static-field".into(), false),
                ("class-field".into(), false),
                ("class-static-blocks".into(), false),
                ("class-private-field".into(), false),
                ("class-private-static-field".into(), false),
                ("class-private-method".into(), false),
                ("class-private-static-method".into(), false),
            ]),
        ),
    ] {
        for minify in [false, true] {
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
                supported: supported.clone(),
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute_node(&result.output_files[0].contents);
        }
    }
}

#[test]
fn class_parameter_defaults_preserve_evaluation_and_all_binding_forms() {
    check_lowering(
        r"
const assert = require('node:assert/strict');
let evaluations = 0;
const evaluate = () => (++evaluations, 13);
const arrow = (Box = class { static value = evaluate(); static self = this; }) => Box;
const expression = function(Box = class { static value = evaluate(); static self = this; }) { return Box; };
function declaration(Box = class { static value = evaluate(); static self = this; }) { return Box; }
const bindingArrow = ({Box = class { static value = evaluate(); static self = this; }}) => Box;
const bindingExpression = function({Box = class { static value = evaluate(); static self = this; }}) { return Box; };
function bindingDeclaration({Box = class { static value = evaluate(); static self = this; }}) { return Box; }
const keyArrow = ({[class {static value = evaluate(); static [Symbol.toPrimitive]() { return 'item'; }}]: value}) => value;
const keyExpression = function({[class {static value = evaluate(); static [Symbol.toPrimitive]() { return 'item'; }}]: value}) { return value; };
function keyDeclaration({[class {static value = evaluate(); static [Symbol.toPrimitive]() { return 'item'; }}]: value}) { return value; }
const classes = [arrow(), expression(), declaration(), bindingArrow({}), bindingExpression({}), bindingDeclaration({})];
for (const Box of classes) { assert.equal(Box.value, 13); assert.equal(Box.self, Box); }
assert.deepEqual([keyArrow({item: 1}), keyExpression({item: 2}), keyDeclaration({item: 3})], [1, 2, 3]);
assert.equal(evaluations, 9);
for (const factory of [arrow, expression, declaration]) assert.equal(factory(null), null);
for (const factory of [bindingArrow, bindingExpression, bindingDeclaration]) assert.equal(factory({Box: null}), null);
assert.equal(evaluations, 9);
assert.notEqual(arrow(), arrow());
assert.equal(evaluations, 11);
const events = [];
function order(first = (events.push('first'), 3), Box = class extends (events.push('extends'), Object) {
  static [(events.push('key'), 'value')] = (events.push('field'), first);
}, last = (events.push('last'), Box.value)) { events.push('body'); return last; }
assert.equal(order(), 3);
assert.deepEqual(events, ['first', 'extends', 'key', 'field', 'last', 'body']);
let bodies = 0;
function throwing(Box = class { static value = (() => { throw new Error('initializer'); })(); }) { ++bodies; }
assert.throws(() => throwing(), /initializer/);
assert.equal(bodies, 0);
throwing(class {});
assert.equal(bodies, 1);
console.log('ok');
",
    );
}

#[test]
fn parameter_class_captures_preserve_receivers_and_reentrant_initialization() {
    check_lowering(
        r"
const assert = require('node:assert/strict');
let Inner, nested = false;
function reenter() { if (!nested) { nested = true; Inner = make(); } return 7; }
function make(Box = class Named {
  static self = this;
  static arrow = () => this;
  static named = () => Named;
  static reentrant = reenter();
  static after = this;
  static { this.block = this; }
  method() { return Named; }
}) { return Box; }
const Outer = make();
assert.notEqual(Outer, Inner);
for (const Box of [Outer, Inner]) {
  assert.equal(Box.self, Box); assert.equal(Box.arrow(), Box); assert.equal(Box.named(), Box);
  assert.equal(Box.after, Box); assert.equal(Box.block, Box); assert.equal(new Box().method(), Box);
}
const receiver = {key: 'outer'};
function lexical(first, Box = class extends (assert.equal(this, receiver), assert.equal(arguments[0], 'arg'), Object) {
  [this.key] = 19;
  static self = this;
  static ordinary = function() { return this; };
  static arrow = () => this;
}) { return Box; }
const Lexical = lexical.call(receiver, 'arg');
assert.equal(new Lexical().outer, 19);
assert.equal(Lexical.self, Lexical);
assert.equal(Lexical.arrow.call(null), Lexical);
assert.equal(Lexical.ordinary.call(receiver), receiver);
function factory(prefix) {
  return (Box = class {static [(assert.equal(arguments[0], 'lexical'), this.key)] = prefix; static self = this; static arrow = () => this;}) => Box;
}
const captured = factory.call(receiver, 'lexical');
const First = captured(), Second = captured();
assert.notEqual(First, Second);
assert.equal(First.outer, 'lexical'); assert.equal(Second.outer, 'lexical');
assert.equal(First.self, First); assert.equal(Second.self, Second);
assert.equal(First.arrow(), First); assert.equal(Second.arrow(), Second);
function nestedDefault(Box = class {static Inner = class {static self = this;}; static self = this;}) { return Box; }
const NestedFirst = nestedDefault(), NestedSecond = nestedDefault();
assert.notEqual(NestedFirst.Inner, NestedSecond.Inner);
assert.equal(NestedFirst.self, NestedFirst); assert.equal(NestedSecond.self, NestedSecond);
assert.equal(NestedFirst.Inner.self, NestedFirst.Inner); assert.equal(NestedSecond.Inner.self, NestedSecond.Inner);
function Construct(Box = class extends (assert.equal(new.target, Construct), Object) {static self = this;}) { this.Box = Box; }
const constructed = new Construct();
assert.equal(constructed.Box.self, constructed.Box);
console.log('ok');
",
    );
}

#[test]
fn parameter_class_computed_keys_and_private_storage_are_per_invocation() {
    check_lowering(
        r"
const assert = require('node:assert/strict');
function keyed(key, Box = class {[key] = 1;}) { return Box; }
const KeyFirst = keyed('first'), KeySecond = keyed('second');
assert.equal(new KeyFirst().first, 1); assert.equal(new KeySecond().second, 1);
let current = 'outer', nested = false, Inner;
function key() {
  const result = current;
  if (!nested) { nested = true; current = 'inner'; Inner = make(); current = 'outer'; }
  return result;
}
function make(Box = class Named {
  [key()] = current;
  #value = current;
  #method() { return this.#value; }
  static #staticValue = current;
  static #staticMethod() { return this.#staticValue; }
  read() { return this.#method(); }
  static read() { return this.#staticMethod(); }
  static self = this;
}) { return Box; }
const Outer = make(), outer = new Outer(), inner = new Inner();
assert.equal(outer.outer, 'outer'); assert.equal(inner.inner, 'outer');
assert.equal(outer.read(), 'outer'); assert.equal(inner.read(), 'outer');
assert.equal(Outer.read(), 'outer'); assert.equal(Inner.read(), 'inner');
assert.equal(Outer.self, Outer); assert.equal(Inner.self, Inner);
current = 'later';
const Later = make(), later = new Later();
assert.equal(later.later, 'later'); assert.equal(later.read(), 'later');
assert.equal(Later.read(), 'later'); assert.equal(Outer.read(), 'outer');
assert.equal(Inner.read(), 'inner'); assert.equal(outer.read(), 'outer');
assert.equal(inner.read(), 'outer');
const another = new Outer();
assert.equal(another.outer, 'later'); assert.equal(another.later, undefined);
assert.equal(another.read(), 'later');
console.log('ok');
",
    );
}
