use std::{collections::HashMap, process::Command};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, BuildTreeShaking, Engine, EngineName,
    Loader, Target, TransformOptions, build, transform,
};

const LOWERED_IMPORTS: &str = r"
var __toESM = 11, __copyProps = 12, __require = 13, __getOwnPropDesc = 14;
var __getOwnPropNames = 15, __getProtoOf = 16, __create = 17, __defProp = 18, __hasOwnProp = 19;
function specifier(label, value) { trace.push('argument:' + label); return value; }
function rejects(promise, error) {
  assert.ok(promise instanceof Promise);
  return promise.then(function() { throw new Error('Expected rejection'); }, function(actual) {
    assert.strictEqual(actual, error);
  });
}
trace.push('before');
var first = import(specifier('first', 'cjs')).then(function(namespace) {
  assert.strictEqual(namespace.default, fixtures.cjs);
  assert.strictEqual(Object.getPrototypeOf(namespace), Object.getPrototypeOf(fixtures.cjs));
  assert.strictEqual(namespace.value, 7);
  fixtures.cjs.value = 8;
  assert.strictEqual(namespace.value, 8);
  assert.strictEqual(Object.getOwnPropertyDescriptor(namespace, 'hidden').enumerable, false);
});
var mutable = 'cjs';
var second = import(mutable).then(function(namespace) {
  assert.strictEqual(namespace.default, 'esm-default');
  assert.strictEqual(namespace.value, 9);
});
mutable = 'esm';
var requireError = import(specifier('require-error', 'throws'));
var argumentError;
try {
  argumentError = import((function() {
    trace.push('argument:argument-error');
    throw fixtures.argumentError;
  })());
} catch (error) { throw new Error('Import argument escaped synchronously: ' + error); }
var helperError = import(specifier('helper-error', 'helper-error'));
var invalid = import(specifier('invalid', 123));
import(specifier('unused', 'cjs'));
trace.push('after');
assert.deepEqual(trace, ['before', 'after']);
Promise.all([
  first, second,
  rejects(requireError, fixtures.requireError),
  rejects(argumentError, fixtures.argumentError),
  rejects(helperError, fixtures.helperError),
  invalid.then(function() { throw new Error('Expected invalid argument rejection'); }, function(error) {
    assert.strictEqual(error.code, 'ERR_INVALID_ARG_TYPE');
  })
]).then(function() {
  assert.deepEqual(trace, [
    'before', 'after', 'argument:first', 'require:cjs', 'require:esm',
    'argument:require-error', 'require:throws', 'argument:argument-error',
    'argument:helper-error', 'require:helper-error', 'argument:invalid', 'require:123',
    'argument:unused', 'require:cjs'
  ]);
  assert.deepEqual([
    __toESM, __copyProps, __require, __getOwnPropDesc, __getOwnPropNames,
    __getProtoOf, __create, __defProp, __hasOwnProp
  ], [11, 12, 13, 14, 15, 16, 17, 18, 19]);
  console.log('ok');
}).catch(function(error) { console.error(error); process.exitCode = 1; });
";

const CONDITIONAL_IMPORTS: &str = r"
function specifier(label, value) { trace.push('argument:' + label); return value; }
var choose = true;
var first = import((trace.push('condition:first'), choose)
  ? specifier('chosen:first', 'cjs') : specifier('unchosen:first', 'throws'));
var second = import((trace.push('condition:second'), !choose)
  ? 'unused' : specifier('chosen:second', 'esm'));
var third = import((trace.push('condition:third'), choose)
  ? 'cjs' : specifier('unchosen:third', 'throws'));
function condition() { trace.push('condition:throws'); throw fixtures.conditionError; }
var fourth;
assert.throws(function() { fourth = import(condition() ? 'cjs' : specifier('unchosen:fourth', 'throws')); },
  function(error) { return error === fixtures.conditionError; });
assert.strictEqual(fourth, undefined);
// Go's syntax minifier extracts the comma from the conditional test before
// import lowering, which moves that entire argument into the callback.
assert.deepEqual(trace, fixtures.minifySyntax ? ['condition:throws']
  : ['condition:first', 'condition:second', 'condition:third', 'condition:throws']);
Promise.all([first, second, third]).then(function(values) {
  assert.strictEqual(values[0].default, fixtures.cjs);
  assert.strictEqual(values[1].default, 'esm-default');
  assert.strictEqual(values[2].default, fixtures.cjs);
  assert.deepEqual(trace, fixtures.minifySyntax ? [
    'condition:throws',
    'condition:first', 'argument:chosen:first', 'require:cjs',
    'condition:second', 'argument:chosen:second', 'require:esm', 'condition:third', 'require:cjs'
  ] : [
    'condition:first', 'condition:second', 'condition:third', 'condition:throws',
    'argument:chosen:first', 'require:cjs', 'argument:chosen:second', 'require:esm', 'require:cjs'
  ]);
  console.log('ok');
}).catch(function(error) { console.error(error); process.exitCode = 1; });
";

fn execute_node(code: &[u8], minify_syntax: bool) {
    let code = serde_json::to_string(&String::from_utf8_lossy(code)).unwrap();
    let harness = format!(
        r"
const assert = require('node:assert/strict');
const nativeRequire = require;
const trace = [];
const fixtures = {{
  minifySyntax: {minify_syntax},
  cjs: Object.assign(Object.create({{ inherited: true }}), {{ value: 7 }}),
  esm: {{ __esModule: true, default: 'esm-default', value: 9 }},
  requireError: new Error('require failed'),
  conditionError: new Error('condition failed'),
  argumentError: new Error('argument failed'),
  helperError: new Error('namespace conversion failed')
}};
Object.defineProperty(fixtures.cjs, 'hidden', {{ value: 10 }});
const helperErrorModule = {{}};
Object.defineProperty(helperErrorModule, '__esModule', {{ get() {{ throw fixtures.helperError; }} }});
function fixtureRequire(path) {{
  trace.push('require:' + path);
  if (path === 'cjs') return fixtures.cjs;
  if (path === 'esm') return fixtures.esm;
  if (path === 'throws') throw fixtures.requireError;
  if (path === 'helper-error') return helperErrorModule;
  return nativeRequire(path);
}}
new Function('require', 'assert', 'trace', 'fixtures', {code})(fixtureRequire, assert, trace, fixtures);
"
    );
    let output = Command::new("node")
        .arg("-e")
        .arg(harness)
        .output()
        .expect("Node.js is required for executable dynamic import regressions");
    assert!(
        output.status.success(),
        "{}\nGenerated code:\n{}",
        String::from_utf8_lossy(&output.stderr),
        code
    );
    assert_eq!(output.stdout, b"ok\n");
}

fn chrome(version: &str) -> Engine {
    Engine {
        name: EngineName::Chrome,
        version: version.into(),
    }
}

fn execute_transform(source: &str, options: TransformOptions) {
    let minify_syntax = options.minify_syntax;
    let result = transform(source, options);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    execute_node(&result.code, minify_syntax);
}

#[test]
fn lowered_dynamic_import_expressions_preserve_evaluation_errors_and_helpers() {
    for version in ["48", "62"] {
        for format in [
            BuildFormat::Default,
            BuildFormat::CommonJs,
            BuildFormat::EsModule,
            BuildFormat::Iife,
        ] {
            for minify in 0..3 {
                execute_transform(
                    LOWERED_IMPORTS,
                    TransformOptions {
                        engines: vec![chrome(version)],
                        format,
                        tree_shaking: BuildTreeShaking::Enabled,
                        minify_whitespace: minify > 0,
                        minify_identifiers: minify == 2,
                        minify_syntax: minify == 2,
                        ..TransformOptions::default()
                    },
                );
            }
        }
    }
}

#[test]
fn bundled_dynamic_import_expressions_include_collision_safe_runtime_require() {
    for version in ["48", "62"] {
        for format in [
            BuildFormat::CommonJs,
            BuildFormat::EsModule,
            BuildFormat::Iife,
        ] {
            for minify in [false, true] {
                let result = build(BuildOptions {
                    stdin: Some(BuildStdin {
                        contents: LOWERED_IMPORTS.into(),
                        loader: Loader::Js,
                        ..BuildStdin::default()
                    }),
                    bundle: true,
                    platform: BuildPlatform::Node,
                    format,
                    engines: vec![chrome(version)],
                    minify_whitespace: minify,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    ..BuildOptions::default()
                });
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                assert_eq!(result.output_files.len(), 1);
                execute_node(&result.output_files[0].contents, minify);
            }
        }
    }
}

#[test]
fn lowered_dynamic_import_conditional_branches_preserve_evaluation_order() {
    for target in [Target::Es5, Target::Es2015] {
        for minify in [false, true] {
            execute_transform(
                CONDITIONAL_IMPORTS,
                TransformOptions {
                    target,
                    supported: [("dynamic-import".into(), false)].into(),
                    minify_whitespace: minify,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    ..TransformOptions::default()
                },
            );
        }
    }
}

#[test]
fn dynamic_import_support_override_controls_expression_lowering() {
    for version in ["48", "63"] {
        execute_transform(
            LOWERED_IMPORTS,
            TransformOptions {
                engines: vec![chrome(version)],
                supported: [("dynamic-import".into(), false)].into(),
                ..TransformOptions::default()
            },
        );
    }
    let native_import = r"
var count = 0;
var name = 'node:path';
var promise = import((count++, name));
assert.strictEqual(count, 1);
promise.then(function(namespace) {
  assert.strictEqual(namespace.default.join('a', 'b'), 'a/b');
  console.log('ok');
}).catch(function(error) { console.error(error); process.exitCode = 1; });
";
    for (version, supported) in [
        ("63", HashMap::default()),
        ("48", [("dynamic-import".into(), true)].into()),
    ] {
        execute_transform(
            native_import,
            TransformOptions {
                engines: vec![chrome(version)],
                supported,
                ..TransformOptions::default()
            },
        );
    }
    execute_transform(
        native_import,
        TransformOptions {
            target: Target::Es2015,
            ..TransformOptions::default()
        },
    );
}
