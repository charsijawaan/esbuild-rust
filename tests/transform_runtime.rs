use std::{
    io::Write,
    process::{Command, Stdio},
};

use esbuild_rs::api::{
    BuildFormat, BuildLegalComments, BuildOptions, BuildPlatform, BuildSourceMap, BuildStdin,
    Loader, Target, TransformOptions, build, transform,
};

const ASYNC_PARAMETERS: &str = r#"
const assert = require('node:assert/strict');
const __async = 11;
function fail() { throw new Error('expected'); }
async function normal(first, value = fail()) { return [first, value, arguments.length]; }
const arrow = async (first, value = fail()) => [first, value];
async function object({value}) { return value; }
const array = async ([value]) => value;
function capture(prefix) { return async (value = fail()) => [this.value, arguments[0], value]; }
function rejected(invoke) {
  const promise = invoke();
  assert.ok(promise instanceof Promise);
  return assert.rejects(promise);
}
assert.equal(normal.length, 1);
assert.equal(arrow.length, 1);
assert.equal(object.length, 1);
assert.equal(array.length, 1);
const captured = capture.call({value: 7}, 'outer');
Promise.all([
  rejected(() => normal(1)),
  rejected(() => arrow(1)),
  rejected(() => object(null)),
  rejected(() => array(null)),
  rejected(() => captured()),
  normal(1, 2).then(value => assert.deepEqual(value, [1, 2, 2])),
  arrow(3, 4).then(value => assert.deepEqual(value, [3, 4])),
  object({value: 5}).then(value => assert.equal(value, 5)),
  array([6]).then(value => assert.equal(value, 6)),
  captured(8).then(value => assert.deepEqual(value, [7, 'outer', 8])),
]).then(() => {
  assert.equal(__async, 11);
  console.log('ok');
}).catch(error => { console.error(error); process.exitCode = 1; });
"#;

fn execute_node(code: &[u8]) {
    static NEXT_FILE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "esbuild-rs-transform-{}-{unique}-{sequence}.cjs",
        std::process::id()
    ));
    std::fs::write(&path, code).unwrap();
    let output = Command::new("node")
        .arg("--enable-source-maps")
        .arg(&path)
        .output()
        .expect("Node.js is required for executable transform regressions");
    std::fs::remove_file(path).unwrap();
    assert!(
        output.status.success(),
        "{}\nGenerated code:\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn default_api_transforms_execute_async_parameter_lowering() {
    for target in [Target::Es2015, Target::Es2016, Target::Es2017] {
        for minify in [false, true] {
            let result = transform(
                ASYNC_PARAMETERS,
                TransformOptions {
                    target,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    keep_names: true,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute_node(&result.code);
        }
    }
}

#[test]
fn default_stdin_transforms_execute_async_parameter_lowering() {
    for minify in [false, true] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(["--target=es2015", "--keep-names"])
            .args(if minify { vec!["--minify"] } else { vec![] })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn esbuild");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(ASYNC_PARAMETERS.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        execute_node(&output.stdout);
    }
}

#[test]
fn default_transforms_include_transitive_lowering_helpers() {
    let source = r#"
const assert = require('node:assert/strict');
const __asyncGenerator = 11, __await = 12, __forAwait = 13, __spreadValues = 14;
class Box { #value = 7; get value() { return this.#value; } }
async function* values() { yield await Promise.resolve(new Box().value); }
async function collect() {
  const result = [];
  for await (const value of values()) result.push(value);
  const source = {a: 1, b: 2};
  const {a, ...rest} = {...source, c: 3};
  assert.equal(a, 1);
  assert.deepEqual(rest, {b: 2, c: 3});
  assert.deepEqual(result, [7]);
  assert.deepEqual([__asyncGenerator, __await, __forAwait, __spreadValues], [11, 12, 13, 14]);
  console.log('ok');
}

collect().catch(error => { console.error(error); process.exitCode = 1; });
"#;
    for minify in [false, true] {
        let result = transform(
            source,
            TransformOptions {
                target: Target::Es2015,
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        execute_node(&result.code);
    }
}

#[test]
fn lowered_default_transforms_preserve_source_maps_and_legal_comments() {
    let source = "/*! retained license */\nasync function mapped() { throw new Error('mapped'); }\n\
                  mapped().catch(error => {\n\
                    if (!error.stack.includes('/virtual/input.js:2:')) throw error;\n\
                    console.log('ok');\n\
                  });";
    for minify in [false, true] {
        let result = transform(
            source,
            TransformOptions {
                sourcefile: "/virtual/input.js".into(),
                target: Target::Es2015,
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                sourcemap: BuildSourceMap::Inline,
                legal_comments: BuildLegalComments::External,
                banner: "// transform banner".into(),
                footer: "// transform footer".into(),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.legal_comments, b"/*! retained license */\n");
        execute_node(&result.code);
    }
}

#[test]
fn lowered_default_transforms_allow_a_parameter_named_arguments() {
    let source = r#"
async function value(arguments = missing()) { return arguments; }
Promise.all([
  value().then(() => { throw new Error('expected rejection'); }, () => {}),
  value(7).then(result => { if (result !== 7) throw new Error('wrong parameter'); }),
]).then(() => console.log('ok')).catch(error => { console.error(error); process.exitCode = 1; });
"#;
    for minify in [false, true] {
        let result = transform(
            source,
            TransformOptions {
                target: Target::Es2015,
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        execute_node(&result.code);
    }
}

#[test]
fn lowered_private_calls_and_template_tags_preserve_receivers() {
    let source = r#"
const assert = require('node:assert/strict');
let receiver, replacement;
const Box = class {
  #method() { return this; }
  #field = function() { return this; };
  get #getter() { receiver = replacement; return function() { return this; }; }
  static #staticMethod() { return this; }
  static #staticField = function() { return this; };
  static get #staticGetter() { return this.#staticField; }
  static check() {
    assert.equal(this.#staticMethod(), this);
    assert.equal(this.#staticMethod``, this);
    assert.equal(this.#staticField(), this);
    assert.equal(this.#staticField``, this);
    assert.equal(this.#staticGetter(), this);
    assert.equal(this.#staticGetter``, this);
  }
  defaultCall(value, result = value.#method()) { return result; }
  defaultTag(value, result = value.#method``) { return result; }
  check() {
    assert.equal(this.#method(), this);
    assert.equal(this.#method``, this);
    assert.equal(this.#field(), this);
    assert.equal(this.#field``, this);
    receiver = this;
    assert.equal(receiver.#getter(), this);
    assert.equal(receiver, replacement);
    receiver = this;
    assert.equal(receiver.#getter``, this);
    assert.equal(receiver, replacement);
    receiver = this;
    assert.equal(receiver.#field(receiver = replacement), this);
    let visits = 0;
    const choose = () => { visits++; return this; };
    assert.equal(choose().#method(), this);
    assert.equal(choose().#method``, this);
    assert.equal(choose().#field(), this);
    assert.equal(choose().#field``, this);
    assert.equal(visits, 4);
    assert.equal(this.defaultCall(this), this);
    assert.equal(this.defaultTag(this), this);
  }
}
replacement = new Box();
new Box().check();
Box.check();
console.log('ok');
"#;
    for target in [Target::Es2015, Target::Es2017, Target::Es2022] {
        for minify in [false, true] {
            let result = transform(
                source,
                TransformOptions {
                    target,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute_node(&result.code);
        }
    }
}

#[test]
fn lowered_static_private_members_initialize_the_captured_class() {
    let source = r#"
const assert = require('node:assert/strict');
const events = [];
class Box {
  static #value = 7;
  static #method() { return this; }
  static #field = function() { return this; };
  static get #getter() { return this.#field; }
  static first = (events.push('first'), this.#value);
  static second = (events.push('second'), this.#method``);
  static check() {
    assert.equal(this.#method(), this);
    assert.equal(this.#method``, this);
    assert.equal(this.#field(), this);
    assert.equal(this.#field``, this);
    assert.equal(this.#getter(), this);
    assert.equal(this.#getter``, this);
    assert.equal(Box, this);
  }
}
const original = Box;
Box = null;
original.check();
assert.equal(original.first, 7);
assert.equal(original.second, original);
assert.deepEqual(events, ['first', 'second']);
class Derived extends original {
  static #value = this;
  static read() { return this.#value; }
}
assert.equal(Derived.read(), Derived);
class Child extends Derived {}
assert.throws(() => Child.read(), TypeError);
console.log('ok');
"#;
    for target in [Target::Es2015, Target::Es2017, Target::Es2022] {
        for minify in [false, true] {
            let result = transform(
                source,
                TransformOptions {
                    target,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    keep_names: true,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute_node(&result.code);
        }
    }
}

#[test]
fn bundled_computed_static_fields_use_the_outer_class_binding() {
    let source = r#"
const events = [];
class Foo {
  [events.push(1)]() {}
  static [events.push(2)] = 123;
  [events.push(3)]() {}
}
if (events.join(',') !== '1,2,3' || Foo[2] !== 123) throw new Error('wrong initialization');
console.log('ok');
"#;
    for target in [Target::Es2015, Target::Es2022] {
        for minify in [false, true] {
            let result = build(BuildOptions {
                bundle: true,
                format: BuildFormat::CommonJs,
                platform: BuildPlatform::Node,
                stdin: Some(BuildStdin {
                    contents: source.into(),
                    sourcefile: "input.ts".into(),
                    loader: Loader::Ts,
                    ..BuildStdin::default()
                }),
                target,
                supported: std::collections::HashMap::from([("class-static-field".into(), false)]),
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            assert_eq!(result.output_files.len(), 1);
            execute_node(&result.output_files[0].contents);
        }
    }
}

#[test]
fn lowered_static_blocks_and_fields_preserve_initialization_order() {
    let source = r#"
const events = [];
class Box {
  #value = 55;
  get #privateGetter() { return this.#value; }
  static first = (events.push(1), 11);
  static #second = (events.push(2), 22);
  static {
    events.push(3);
    this.total = this.#method();
    this.saved = () => this;
    this.savedDeep = ({ value = this } = {}) => {
      const object = { [this.first]: this, nested: () => this, method() { return this; } };
      try {
        if (value === this && object[this.first] === this && object.nested() === this
          && object.method() === object) return this;
        throw value;
      } catch { return null; }
    };
    this.ordinary = function() { return this; };
    this.instanceValue = (new this).#privateGetter;
    this.brands = #second in this && #method in this && !(#value in this);
  }
  static fourth = (events.push(4), 44);
  static #method() { return this.first + this.#second; }
}
const Captured = Box;
Box = null;
if (events.join(',') !== '1,2,3,4' || Captured.total !== 33 || Captured.fourth !== 44
  || Captured.saved() !== Captured || Captured.savedDeep() !== Captured
  || Captured.ordinary.call(events) !== events || Captured.instanceValue !== 55 || !Captured.brands)
  throw new Error('wrong static initialization');
console.log('ok');
"#;
    for source in [
        source.to_owned(),
        source.replace("class Box {", "let Box = class {"),
    ] {
        for (target, lower_blocks) in [
            (Target::Es2015, false),
            (Target::Es2021, false),
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
                    &source,
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
                        contents: source.clone(),
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
                execute_node(&result.output_files[0].contents);
            }
        }
    }
}
