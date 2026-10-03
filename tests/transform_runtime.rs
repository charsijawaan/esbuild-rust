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

#[test]
fn typescript_assignment_fields_evaluate_computed_keys_once_in_source_order() {
    let source = r#"
const events = [];
const key = value => (events.push(value), value);
const init = (event, value) => (events.push(event), value);
const base = event => (events.push(event), class {});
class Foo extends base('base') {
  [key('a')]() {}
  [key('b')];
  [key('c')] = init('instance', 1);
  ['literal'] = 7;
  [3] = 8;
  [key('d')]() {}
  static [key('e')];
  static [key('f')] = init('static', 2);
  static [key('g')]() {}
  [key('h')];
}
const Bar = class extends base('expr-base') {
  static [key('i')];
  static [key('j')] = init('expr-static', 3);
  [key('k')] = init('expr-instance', 4);
  [key('l')];
};
const expected = 'base,a,b,c,d,e,f,g,h,static,expr-base,i,j,k,l,expr-static';
if (events.join(',') !== expected) throw new Error(events.join(','));
const first = new Foo, second = new Foo, third = new Bar;
if (events.join(',') !== expected + ',instance,instance,expr-instance')
  throw new Error('keys evaluated during construction: ' + events.join(','));
if (first.c !== 1 || second.c !== 1 || first.literal !== 7 || first[3] !== 8
  || Foo.f !== 2 || Bar.j !== 3 || third.k !== 4
  || 'b' in first || 'h' in first || 'e' in Foo || 'i' in Bar || 'l' in third)
  throw new Error('wrong assignment fields');
console.log('ok');
"#;
    let tsconfig = r#"{"compilerOptions":{"useDefineForClassFields":false}}"#;
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
                source,
                TransformOptions {
                    loader: Loader::Ts,
                    tsconfig_raw: tsconfig.into(),
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
                    loader: Loader::Ts,
                    ..BuildStdin::default()
                }),
                tsconfig_raw: tsconfig.into(),
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

#[test]
fn lowered_private_storage_is_local_to_each_factory_call() {
    let source = r#"
function make(value) {
  const _field = 11, method_fn = 12;
  class Local {
    #field = value;
    static #seed = value;
    #method() { return [Local, this.#field]; }
    get #access() { return this.#field; }
    set #access(next) { this.#field = next; }
    read() { return this.#method(); }
    increment() { this.#access += 1; return this.#access; }
    static seed() { return this.#seed; }
  }
  const Original = Local;
  Local = null;
  if (_field !== 11 || method_fn !== 12) throw 'name collision';
  return Original;
}
function expression(value) {
  return class {
    #field = value;
    static #seed = value;
    #method() { return this.#field; }
    read() { return this.#method(); }
    static seed() { return this.#seed; }
  };
}
const arrow = value => class {
  #field = value;
  #method() { return this.#field; }
  read() { return this.#method(); }
};
const A = make(1), a = new A, B = make(2), b = new B;
if (a.read()[0] !== A || a.read()[1] !== 1 || b.read()[0] !== B || b.read()[1] !== 2
  || A.seed() !== 1 || B.seed() !== 2 || a.increment() !== 2 || b.read()[1] !== 2)
  throw 'shared declaration storage';
let rejected = false;
try { A.prototype.read.call(b); } catch (error) { rejected = error instanceof TypeError; }
if (!rejected) throw 'shared private brands';
const C = expression(3), c = new C, D = expression(4), d = new D;
if (c.read() !== 3 || d.read() !== 4 || C.seed() !== 3 || D.seed() !== 4)
  throw 'shared expression storage';
const E = arrow(5), e = new E, F = arrow(6), f = new F;
if (e.read() !== 5 || f.read() !== 6) throw 'shared arrow storage';
console.log('ok');
"#;
    execute_node(source.as_bytes());
    for (target, lower_private) in [
        (Target::Es2015, false),
        (Target::Es2022, false),
        (Target::Es2022, true),
    ] {
        for minify in [false, true] {
            let supported = if lower_private {
                std::collections::HashMap::from([
                    ("class-private-field".into(), false),
                    ("class-private-method".into(), false),
                    ("class-private-accessor".into(), false),
                    ("class-private-static-field".into(), false),
                ])
            } else {
                std::collections::HashMap::new()
            };
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

#[test]
fn lowered_private_members_retain_the_original_class_binding() {
    let source = r#"
let saved, active;
class Box {
  static observedName = this.name;
  #initial = Box;
  #method() { return Box; }
  get #reader() { return Box; }
  set #writer(value) { saved = [Box, value]; }
  read() { return [this.#initial, this.#method(), this.#reader]; }
  write(value) { this.#writer = value; }
}
const Original = Box;
Box = null;
const box = new Original;
if (box.read().some(value => value !== Original)) throw 'wrong class binding';
box.write(17);
if (saved[0] !== Original || saved[1] !== 17) throw 'wrong setter binding';
class Compound {
  get #field() { active = new Compound; return this.result; }
  set #field(value) { this.result = value; }
  multiply() { active = this; active.result = 2; active.#field *= 3; }
}
const OriginalCompound = Compound;
Compound = null;
const compound = new OriginalCompound;
compound.multiply();
if (compound === active || compound.result !== 6 || active.result !== undefined)
  throw 'wrong compound receiver';
if (Original.name !== 'Box' || Original.observedName !== 'Box'
  || OriginalCompound.name !== 'Compound') throw 'wrong name';
console.log('ok');
"#;
    for (target, lower_private, lower_blocks) in [
        (Target::Es2015, false, false),
        (Target::Es2022, false, false),
        (Target::Es2022, true, false),
        (Target::Es2022, true, true),
    ] {
        for minify in [false, true] {
            let mut supported = if lower_private {
                std::collections::HashMap::from([
                    ("class-private-field".into(), false),
                    ("class-private-method".into(), false),
                    ("class-private-accessor".into(), false),
                ])
            } else {
                std::collections::HashMap::new()
            };
            if lower_blocks {
                supported.insert("class-static-blocks".into(), false);
            }
            let result = transform(
                source,
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
                keep_names: true,
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
fn lowered_private_optional_calls_preserve_receivers_and_short_circuiting() {
    let source = r#"
let visits = 0, argumentsEvaluated = 0, getterReads = 0, mutableReceiver;
const arg = () => (++argumentsEvaluated, mutableReceiver = null, 7);
const same = (actual, expected) => {
  if (actual !== expected) throw new Error('wrong receiver or result');
};
class Base {
  make() { if (this.value !== 123) throw new Error('wrong super receiver'); return this; }
}
class Box extends Base {
  value = 123;
  self = this;
  #empty;
  #field = function(value) { if (this.value !== 123 || value !== 7) throw 'field'; return this; };
  #method(value) { if (this.value !== 123 || value !== 7) throw 'method'; return this; }
  get #getter() { ++getterReads; mutableReceiver = null; return this.#field; }
  check() { if (this.value !== 123) throw 'public method'; return this; }
  withDefault(value = this?.#method?.(arg())) { return value; }
  withGetterDefault(value = (mutableReceiver = this)?.#getter?.(arg())) { return value; }
  run() {
    let receiver = this;
    const that = () => (++visits, receiver);
    same(that().#method(arg()), this);
    same(that().#method?.(arg()), this);
    same(that()?.#method(arg()), this);
    same(that()?.#method?.(arg()), this);
    same(that().self.#method(arg()), this);
    same(that().self.#method?.(arg()), this);
    same(that().self?.#method(arg()), this);
    same(that().self?.#method?.(arg()), this);
    same(that()?.self.#method(arg()), this);
    same(that()?.self.#method?.(arg()), this);
    same(that()?.self?.#method(arg()), this);
    same(that()?.self?.#method?.(arg()), this);
    if (visits !== 12 || argumentsEvaluated !== 12) throw 'repeated evaluation';
    receiver = null;
    same(that()?.#method?.(arg()), undefined);
    same(that()?.self.#method?.(arg()), undefined);
    same(that()?.self?.#getter?.(arg()), undefined);
    same(this.#empty?.(arg()), undefined);
    if (visits !== 15 || argumentsEvaluated !== 12 || getterReads !== 0) throw 'short circuit';
    mutableReceiver = this;
    same(mutableReceiver.#getter?.(arg()), this);
    mutableReceiver = this;
    same(mutableReceiver?.#getter?.(arg()), this);
    same(this.withDefault(), this);
    same(this.withGetterDefault(), this);
    same(this?.#method?.(arg()).self?.#method?.(arg()).check?.(), this);
    same(super.make?.().#method?.(arg()), this);
    let threw = false;
    try { (receiver?.self).#getter?.(arg()); } catch (error) { threw = error instanceof TypeError; }
    if (!threw || getterReads !== 3) throw 'private access boundary';
  }
}
new Box().run();
console.log('ok');
"#;
    // Native execution also covers receiver mutation and nested parameter
    // captures that the pinned Go lowering currently handles incorrectly.
    execute_node(source.as_bytes());
    for (target, lower_private) in [
        (Target::Es2015, false),
        (Target::Es2022, false),
        (Target::Es2022, true),
    ] {
        for minify in [false, true] {
            let supported = if lower_private {
                std::collections::HashMap::from([
                    ("class-private-field".into(), false),
                    ("class-private-method".into(), false),
                    ("class-private-accessor".into(), false),
                ])
            } else {
                std::collections::HashMap::new()
            };
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
