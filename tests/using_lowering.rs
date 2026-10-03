use std::{collections::HashMap, process::Command};

use esbuild_rs::api::{BuildFormat, BuildPlatform, Loader, TransformOptions, transform};

fn run(source: &str, flags: &[(&str, bool)], minify: bool) -> String {
    let result = transform(
        source,
        TransformOptions {
            sourcefile: "input.js".into(),
            format: BuildFormat::EsModule,
            platform: BuildPlatform::Node,
            supported: flags
                .iter()
                .map(|(name, value)| (name.to_string(), *value))
                .collect::<HashMap<_, _>>(),
            minify_syntax: minify,
            minify_identifiers: minify,
            minify_whitespace: minify,
            ..TransformOptions::default()
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let code = String::from_utf8(result.code).expect("generated JavaScript");
    if flags
        .iter()
        .any(|&(name, value)| (name == "using" || name == "async-await") && !value)
    {
        assert!(!code.contains("await using "), "{code}");
    }
    if flags.iter().any(|&(name, value)| name == "using" && !value) {
        assert!(
            !regex::Regex::new(r"\busing\s+").unwrap().is_match(&code),
            "{code}"
        );
    }
    let output = Command::new("node")
        .args(["--input-type=module", "-e", &code])
        .output()
        .expect("execute disposal assertions");
    assert!(
        output.status.success(),
        "flags={flags:?}, minify={minify}: {}\n{code}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok\n");
    code
}

#[test]
fn lowers_sync_resources_across_nested_scopes_returns_and_loop_exits() {
    let source = r#"
import assert from 'node:assert/strict';
const events = [];
const resource = id => ({ id, [Symbol.dispose]() { events.push(id); } });
function scope() {
  using outer = resource('outer');
  { using a = resource('a'), b = resource('b'); events.push('body'); }
  return outer.id;
}
assert.equal(scope(), 'outer');
assert.deepEqual(events.splice(0), ['body', 'b', 'a', 'outer']);
for (using item of [resource('first'), resource('second'), resource('third')]) {
  events.push('body:' + item.id);
  if (item.id === 'first') continue;
  break;
}
assert.deepEqual(events.splice(0), ['body:first', 'first', 'body:second', 'second']);
try {
  for (using item of [resource('throw')]) { throw 'loop'; }
} catch (error) { assert.equal(error, 'loop'); }
assert.deepEqual(events.splice(0), ['throw']);
try {
  using first = resource('before-error');
  using invalid = 1;
} catch (error) { assert.equal(error.name, 'TypeError'); }
assert.deepEqual(events.splice(0), ['before-error']);
{ using empty = null; using other = undefined; }
console.log('ok');
"#;
    for minify in [false, true] {
        run(source, &[("using", false)], minify);
    }
}

#[test]
fn lowers_async_resources_in_functions_generators_and_for_of_loops() {
    let source = r#"
import assert from 'node:assert/strict';
async function main() {
const events = [];
const resource = id => ({ id, async [Symbol.asyncDispose]() { await 0; events.push(id); } });
async function fn() {
  await using outer = resource('outer');
  { await using first = resource('first'), second = resource('second'); events.push('body'); }
  for (await using item of [resource('loop1'), resource('loop2')]) {
    if (item.id === 'loop1') continue;
    break;
  }
  return outer.id;
}
assert.equal(await fn(), 'outer');
assert.deepEqual(events.splice(0), ['body', 'second', 'first', 'loop1', 'loop2', 'outer']);
async function* generator() {
  await using value = resource('generator');
  yield value.id;
  events.push('after-yield');
}
const iterator = generator();
assert.deepEqual(await iterator.next(), { value: 'generator', done: false });
assert.deepEqual(events, []);
assert.deepEqual(await iterator.return(), { value: undefined, done: true });
assert.deepEqual(events.splice(0), ['generator']);
for await (await using value of [resource('iteration1'), resource('iteration2')]) {
  events.push('body:' + value.id);
}
assert.deepEqual(events.splice(0), ['body:iteration1', 'iteration1', 'body:iteration2', 'iteration2']);
console.log('ok');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
"#;
    for flags in [
        vec![("using", false)],
        vec![("async-await", false)],
        vec![("async-generator", false)],
        vec![
            ("using", false),
            ("async-await", false),
            ("async-generator", false),
            ("for-await", false),
        ],
    ] {
        for minify in [false, true] {
            run(source, &flags, minify);
        }
    }
}

#[test]
fn disposal_errors_preserve_suppression_and_async_fallback_ordering() {
    let source = r#"
import assert from 'node:assert/strict';
async function main() {
const events = [];
try {
  using first = { [Symbol.dispose]() { throw 'first'; } };
  using second = { [Symbol.dispose]() { throw 'second'; } };
  throw 'body';
} catch (error) {
  assert.equal(error.name, 'SuppressedError');
  assert.equal(error.error, 'first');
  assert.equal(error.suppressed.error, 'second');
  assert.equal(error.suppressed.suppressed, 'body');
}

async function fn() {
  const pending = new Promise(resolve => setTimeout(() => { events.push('promise'); resolve(); }, 0));
  {
    await using first = { async [Symbol.asyncDispose]() { events.push('async'); } };
    await using second = { [Symbol.dispose]() { events.push('sync'); return pending; } };
  }
  events.push('body');
  await pending;
  assert.deepEqual(events.splice(0), ['sync', 'async', 'body', 'promise']);
  const interleave = Promise.resolve().then(() => events.push('interleave'));
  try {
    await using value = { [Symbol.dispose]() { events.push('dispose'); throw null; } };
  } catch (error) { assert.equal(error, null); events.push('catch'); }
  await interleave;
  assert.deepEqual(events.splice(0), ['dispose', 'interleave', 'catch']);
  await using empty = null;
}
await fn();
console.log('ok');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
"#;
    for minify in [false, true] {
        run(source, &[("using", false), ("async-await", false)], minify);
    }
}

#[test]
fn module_disposal_preserves_exports_and_hoisted_function_access() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let source = r#"
const events = [];
using value = { id: 7, [Symbol.dispose]() { events.push('module'); } };
export { events };
export const number = 42;
export let counter = 1;
export function increment() { counter++; }
export class C { static self = C; read() { return value.id; } }
export function get() { return [value.id, new C().read(), C.self === C]; }
export default { value, get };
"#;
    for minify in [false, true] {
        let result = transform(
            source,
            TransformOptions {
                format: BuildFormat::EsModule,
                supported: HashMap::from([("using".into(), false)]),
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let url = format!(
            "data:text/javascript;base64,{}",
            STANDARD.encode(result.code)
        );
        let runner = format!(
            r#"
import assert from 'node:assert/strict';
const ns = await import('{url}');
assert.deepEqual(ns.events, ['module']);
assert.equal(ns.number, 42);
assert.equal(ns.counter, 1);
ns.increment();
assert.equal(ns.counter, 2);
assert.deepEqual(ns.get(), [7, 7, true]);
assert.equal(ns.default.value.id, 7);
assert.equal(ns.default.get, ns.get);
console.log('ok');
"#
        );
        let output = Command::new("node")
            .args(["--input-type=module", "-e", &runner])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"ok\n");
    }
}

#[test]
fn typescript_resources_preserve_namespace_enum_and_default_exports() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let cases = [
        (
            "using x = resource(); export namespace N { export const value = x.id; }",
            "assert.equal(ns.N.value, 7);",
        ),
        (
            "using x = resource(); export enum E { A = x.id }",
            "assert.equal(ns.E.A, 7); assert.equal(ns.E[7], 'A');",
        ),
        (
            "export namespace N { using x = resource(); export const value = x.id; export function get() { return value; } }",
            "assert.equal(ns.N.value, 7); assert.equal(ns.N.get(), 7);",
        ),
        (
            "export namespace N { using x = resource(); export enum E { A = x.id } export namespace M { export const value = x.id; } }",
            "assert.equal(ns.N.E.A, 7); assert.equal(ns.N.M.value, 7);",
        ),
        (
            "using x = resource(); export default class C { read() { return x.id; } }",
            "assert.equal(new ns.default().read(), 7);",
        ),
        (
            "using x = resource(); export default function get() { return x.id; }",
            "assert.equal(ns.default(), 7);",
        ),
        (
            "using x = resource(); export const { a, b: c } = { a: x.id, b: 8 };",
            "assert.equal(ns.a, 7); assert.equal(ns.c, 8);",
        ),
    ];
    for (source, assertions) in cases {
        let source = format!(
            "export const events = []; function resource() {{ return {{ id: 7, [Symbol.dispose]() {{ events.push('disposed'); }} }}; }} {source}"
        );
        for minify in [false, true] {
            let result = transform(
                &source,
                TransformOptions {
                    loader: Loader::Ts,
                    format: BuildFormat::EsModule,
                    supported: HashMap::from([("using".into(), false)]),
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            let url = format!(
                "data:text/javascript;base64,{}",
                STANDARD.encode(result.code)
            );
            let runner = format!(
                "import assert from 'node:assert/strict'; const ns = await import('{url}'); {assertions} assert.deepEqual(ns.events, ['disposed']); console.log('ok');"
            );
            let output = Command::new("node")
                .args(["--input-type=module", "-e", &runner])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "source={source}, minify={minify}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"ok\n");
        }
    }
}
