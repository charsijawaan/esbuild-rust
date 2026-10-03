use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Loader, Target, TransformOptions, build,
    transform,
};

const DESTRUCTURING: &str = r#"
const assert = require('node:assert/strict');
function plain() {
  let trail = [];
  let t = key => (trail.push(key), key);
  let [
    { [t('a')]: a } = { a: t('x') },
    { [t('b')]: b, ...c } = { b: t('y') },
    { [t('d')]: d } = { d: t('z') },
  ] = [{ a: 1 }, { b: 2, bb: 3 }];
  return JSON.stringify({ a, b, c, d, trail });
}
namespace Pattern {
  let trail = [];
  let t = key => (trail.push(key), key);
  export let [
    { [t('a')]: a } = { a: t('x') },
    { [t('b')]: b, ...c } = { b: t('y') },
    { [t('d')]: d } = { d: t('z') },
  ] = [{ a: 1 }, { b: 2, bb: 3 }];
  export let result = JSON.stringify({ a, b, c, d, trail });
}
assert.equal(Pattern.result, plain());
const obj = {};
({ a: obj.a, ...obj.b } = { a: 1, b: 2, c: 3 });
[obj.c, , ...obj.d] = [1, 2, 3];
({ e: obj.e, f: obj.f = 'f' } = { e: 'e' });
[obj.g, , obj.h = 'h'] = ['g', 'gg'];
namespace Defaults {
  export let { a, ...b } = { a: 1, b: 2, c: 3 };
  export let [c, , ...d] = [1, 2, 3];
  export let { e, f = 'f' } = { e: 'e' };
  export let [g, , h = 'h'] = ['g', 'gg'];
}
assert.equal(JSON.stringify(Defaults), JSON.stringify(obj));
namespace Updates {
  export let { value } = { value: 1 };
  export function read() { return { value }; }
  export function write(next) { ({ value } = { value: next }); }
}
Updates.write(3);
assert.equal(Updates.value, 3);
assert.deepEqual(Updates.read(), { value: 3 });
let z = { x: { x: 'x' }, y: 'y' }, { [(z = { z: 'z' }, 'x')]: x, ...y } = z;
assert.equal(x.x, 'x');
assert.equal(y.y, 'y');
assert.equal(z.z, 'z');
function argumentsWithComma({ [(z = { changed: true }, 'key')]: value }) {
  return value;
}
assert.equal(argumentsWithComma({ key: 9 }), 9);
assert.equal(z.changed, true);
console.log('ok');
"#;

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for destructuring printer regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn destructuring_preserves_namespace_aliases_and_computed_key_parentheses() {
    for target in [Target::Es2015, Target::Es2022] {
        for minify in [false, true] {
            let result = transform(
                DESTRUCTURING,
                TransformOptions {
                    loader: Loader::Ts,
                    target,
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
                    contents: DESTRUCTURING.into(),
                    loader: Loader::Ts,
                    ..BuildStdin::default()
                }),
                format: BuildFormat::CommonJs,
                platform: BuildPlatform::Node,
                target,
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
