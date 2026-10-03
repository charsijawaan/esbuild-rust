use std::{
    io::Write,
    process::{Command, Stdio},
};

use esbuild_rs::api::{Loader, Target, TransformOptions, transform};

const RUNTIME_SOURCE: &str = r"
const assert = require('node:assert/strict');
enum Text {
  Empty = '', Ascii = 'abc', Bmp = 'ȧḃċ', Cluster = '👯‍♂️',
  Pair = '\ud83d\ude00', High = '\ud83d', Low = '\ude00', Nul = '\0a',
  Joined = Ascii + Bmp, Template = `${Ascii}:${Bmp}`,
}
assert.deepEqual([
  Text.Empty.length, Text.Ascii.length, Text.Bmp.length, Text.Cluster.length,
  Text.Pair.length, Text.High.length, Text.Low.length, Text.Nul.length,
  Text.Joined.length, Text.Template['length'], Text['Cluster']['length'],
], [0, 3, 3, 5, 2, 1, 1, 2, 6, 7, 5]);
assert.deepEqual([
  ''.length, 'abc'.length, 'ȧḃċ'.length, '👯‍♂️'.length, `😀`.length,
  '\ud800'.length, '\udc00'.length, '\0a'['length'], `${'abc'}:${'ȧḃċ'}`.length,
], [0, 3, 3, 5, 2, 1, 1, 2, 7]);
assert.deepEqual([
  'abc'[-0], 'abc'[0], 'abc'[2], '😀'[0], '😀'[1], '\ud800'[0], '\udc00'[0],
  'ȧḃċ'[1], '\0a'[0], 'abc'['0'], Text.Ascii[0], `😀`[1],
], ['a', 'a', 'c', '\ud83d', '\ude00', '\ud800', '\udc00', 'ḃ', '\0', 'a', 'a', '\ude00']);
assert.deepEqual([
  'abc'[NaN], 'abc'[Infinity], 'abc'[-Infinity], 'abc'[1e100], 'abc'[-1e100],
  'abc'[4294967296], 'abc'[4294967297], 'abc'[BigInt(1)],
], [undefined, undefined, undefined, undefined, undefined, undefined, undefined, 'b']);
let reads = 0;
for (const key of ['-1', '3', '0.5', '00']) {
  Object.defineProperty(String.prototype, key, {
    configurable: true, get() { ++reads; return key; },
  });
}
assert.deepEqual(['abc'[-1], 'abc'[3], 'abc'[0.5], 'abc'['00']], ['-1', '3', '0.5', '00']);
assert.equal(reads, 4);
for (const key of ['-1', '3', '0.5', '00']) delete String.prototype[key];
let tagCalls = 0;
function tag() {
  ++tagCalls;
  return { get length() { ++reads; return 19; }, 0: 'tagged' };
}
assert.equal(tag`abc`.length, 19);
assert.equal(tag`abc`[0], 'tagged');
assert.equal(tagCalls, 2);
assert.equal(reads, 5);
let callCount = 0;
Object.defineProperty(String.prototype, '3', {
  configurable: true,
  get() {
    ++reads;
    return function(arg) {
      'use strict';
      ++callCount;
      assert.equal(this, 'abc');
      return arg;
    };
  },
});
assert.equal('abc'[3]('call'), 'call');
assert.equal('abc'[3]`tagged`[0], 'tagged');
assert.equal(callCount, 2);
assert.equal(reads, 7);
delete String.prototype[3];
for (const invoke of [
  () => 'abc'[0](), () => 'abc'[0]`tagged`, () => 'abc'.length`tagged`,
]) assert.throws(invoke, TypeError);
Text.Ascii[0] = 'x';
Text.Ascii[0]++;
assert.equal(Text.Ascii[0], 'a');
assert.equal(delete Text.Ascii[0], false);
Text.Ascii.length = 99;
Text.Ascii.length++;
assert.equal(Text.Ascii.length, 3);
console.log('ok');
";

#[test]
fn matches_pinned_go_utf16_and_computed_length_output() {
    for (source, expected) in [
        ("a = '😀'[0]", "a = \"\\uD83D\";\n"),
        ("a = '😀'[1]", "a = \"\\uDE00\";\n"),
        ("a = '\\ud800'.length", "a = 1;\n"),
        ("a = '\\udc00'[0]", "a = \"\\uDC00\";\n"),
        ("a = ('abc'+'😀').length", "a = 5;\n"),
        ("a = 'abc'['length']", "a = 3;\n"),
        ("a = 'abc'['len'+'gth']", "a = 3;\n"),
        ("a = 'abc'[0.5]", "a = \"abc\"[0.5];\n"),
        ("a = 'abc'?.length", "a = 3;\n"),
        ("a = 'abc'?.[0]", "a = \"a\";\n"),
    ] {
        let result = transform(
            source,
            TransformOptions {
                minify_syntax: true,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(String::from_utf8_lossy(&result.code), expected, "{source}");
    }
}

#[test]
fn matches_pinned_go_literal_string_index_context_quirks() {
    // Pinned Go folds numeric literal-string indexes even in write/delete
    // contexts. These assert output parity only: the writes become invalid
    // JavaScript, and the deletes change a non-configurable property's result.
    for (source, expected) in [
        ("'abc'[0]=1", "\"a\" = 1;\n"),
        ("'abc'[0]++", "\"a\"++;\n"),
        ("delete 'abc'[0]", "delete \"a\";\n"),
        ("delete 'abc'?.[0]", "delete \"a\";\n"),
        ("'abc'[0]()", "\"a\"();\n"),
        ("'abc'[0]`tagged`", "\"a\"`tagged`;\n"),
        ("'abc'.length`tagged`", "3`tagged`;\n"),
    ] {
        let result = transform(
            source,
            TransformOptions {
                minify_syntax: true,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(String::from_utf8_lossy(&result.code), expected, "{source}");
    }
}

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for string and enum runtime regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn enum_string_index_optional_delete_preserves_native_runtime_behavior() {
    // Lowered optional delete has a separate existing visitor gap. Check the
    // inlined-enum wrapper's supported native behavior here.
    let source = "const assert = require('node:assert/strict'); enum Text { Value = 'abc' }; assert.equal(delete Text.Value?.[0], false); console.log('ok');";
    for minify in [false, true] {
        let result = transform(
            source,
            TransformOptions {
                loader: Loader::Ts,
                target: Target::EsNext,
                minify_syntax: minify,
                minify_identifiers: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        execute(&result.code);
    }
}

#[test]
fn string_and_enum_folding_preserves_utf16_and_runtime_boundaries() {
    for (target, target_name) in [
        (Target::Es2015, "es2015"),
        (Target::Es2020, "es2020"),
        (Target::EsNext, "esnext"),
    ] {
        for mode in 0..3 {
            let result = transform(
                RUNTIME_SOURCE,
                TransformOptions {
                    loader: Loader::Ts,
                    target,
                    minify_syntax: mode != 0,
                    minify_identifiers: mode == 2,
                    minify_whitespace: mode == 2,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute(&result.code);

            let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
                .arg("--loader=ts")
                .arg(format!("--target={target_name}"))
                .args(match mode {
                    0 => vec![],
                    1 => vec!["--minify-syntax"],
                    _ => vec!["--minify"],
                })
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(RUNTIME_SOURCE.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            execute(&output.stdout);
        }
    }
}
