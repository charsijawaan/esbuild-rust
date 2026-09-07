#!/usr/bin/env node

// Inventory source-level test definitions separately from captured fixture cases
// and runtime results. Registration is not execution or a parity claim.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import Module, { createRequire } from 'node:module';

const [upstreamArg, outputArg] = process.argv.slice(2);
if (!upstreamArg || !outputArg || process.argv.length !== 4) {
  console.error('usage: node scripts/audit_upstream_test_inventory.mjs <upstream-root> <output-json>');
  process.exit(2);
}
const upstreamRoot = path.resolve(upstreamArg);
const pin = fs.readFileSync(new URL('../UPSTREAM.md', import.meta.url), 'utf8').match(/Commit: `([a-f0-9]+)`/)[1];
const git = (...args) => execFileSync('git', args, { cwd: upstreamRoot, encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
if (git('rev-parse', 'HEAD').trim() !== pin) throw new Error(`Expected pinned upstream ${pin}`);
const files = git('ls-tree', '-r', '--name-only', pin).trim().split('\n');
const fixtureCounts = new Map();
const add = file => fixtureCounts.set(file, (fixtureCounts.get(file) || 0) + 1);
const readFixture = name => JSON.parse(fs.readFileSync(new URL(`../tests/upstream/${name}.json`, import.meta.url), 'utf8'));
for (const [fixture, source] of Object.entries({
  css_lexer: 'internal/css_lexer/css_lexer_test.go',
  css_parser: 'internal/css_parser/css_parser_test.go',
  css_printer: 'internal/css_printer/css_printer_test.go',
  js_lexer: 'internal/js_lexer/js_lexer_test.go',
  js_printer: 'internal/js_printer/js_printer_test.go',
  json_parser: 'internal/js_parser/json_parser_test.go',
})) for (const _ of readFixture(fixture)) add(source);
for (const item of readFixture('js_parser')) add(`internal/js_parser/${item.file}`);
for (const item of readFixture('bundler')) add(`internal/bundler_tests/bundler_${item.suite}_test.go`);
for (const item of readFixture('api')) add(`pkg/api/${item.file}`);

const goFiles = files.filter(file => file.endsWith('_test.go')).map(file => {
  const source = git('show', `${pin}:${file}`);
  const tests = [...source.matchAll(/^func (Test\w+)\(t \*testing\.T\)/gm)].map(match => ({
    name: match[1], line: source.slice(0, match.index).split('\n').length,
  }));
  if ([...source.matchAll(/^func (Test\w+)\(/gm)].filter(match => match[1] !== 'TestMain').length !== tests.length) {
    throw new Error(`Unrecognized Go test declaration in ${file}`);
  }
  const captured = fixtureCounts.get(file) || 0;
  return { file, test_function_count: tests.length, captured_fixture_cases: captured,
    correspondence: captured ? 'captured-fixtures-not-a-pass-count' : tests.length ? 'requires-correspondence-audit' : 'test-infrastructure' };
});
for (const file of fixtureCounts.keys()) {
  if (!goFiles.some(item => item.file === file)) throw new Error(`Fixture source missing from upstream: ${file}`);
}

const registrations = {
  'scripts/js-api-tests.js': ['buildTests', 'watchTests', 'serveTests', 'transformTests', 'formatTests', 'analyzeTests', 'apiSyncTests', 'childProcessTests', 'serialTests'],
  'scripts/plugin-tests.js': ['pluginTests', 'syncTests'],
  'scripts/wasm-tests.js': ['tests'],
};
const jsSuites = [];
for (const [file, groupNames] of Object.entries(registrations)) {
  const filename = path.join(upstreamRoot, file);
  let source = git('show', `${pin}:${file}`);
  const mainCall = '\nmain().catch(e => setTimeout(() => { throw e }))\n';
  if (!source.endsWith(mainCall)) throw new Error(`Unrecognized entry point in ${file}`);
  source = source.slice(0, -mainCall.length) + `\nmodule.exports = { ${groupNames.join(', ')} };\n`;
  const module = new Module(filename);
  module.filename = filename;
  const require = createRequire(filename);
  // Any attempted install/build/cleanup during registration is a hard error.
  const helpers = new Proxy({}, { get: (_, name) => () => { throw new Error(`Unexpected helper call during registration: ${String(name)}`); } });
  module.require = name => name === './esbuild' || name === './esbuild.js' ? helpers : require(name);
  module._compile(source, filename);
  const groups = Object.entries(module.exports).map(([name, tests]) => {
    if (Object.values(tests).some(test => typeof test !== 'function')) throw new Error(`Unrecognized test registration in ${file}:${name}`);
    return { name, count: Object.keys(tests).length };
  });
  jsSuites.push({ file, registered_tests: groups.reduce((n, group) => n + group.count, 0),
    status: 'registered-only-not-executed-against-port', groups });
}
const otherScripts = files.filter(file => file.startsWith('scripts/') && /test|fuzz/i.test(path.basename(file))
  && /\.(js|ts|css|html)$/.test(file)
  && !Object.hasOwn(registrations, file)).map(file => ({ file,
    status: file === 'scripts/end-to-end-tests.js' ? 'separate-runtime-report' : 'not-enumerated' }));
const inventory = {
  upstream_revision: pin, node_version: process.version, platform: process.platform, architecture: process.arch,
  go_test_files: goFiles.length,
  go_test_functions: goFiles.reduce((n, file) => n + file.test_function_count, 0),
  captured_fixture_cases: [...fixtureCounts.values()].reduce((a, b) => a + b, 0),
  counting_note: 'Go function definitions, captured helper calls, and JavaScript registrations are different units. Do not add them into a parity denominator.',
  go_files: goFiles, javascript_suites: jsSuites, other_test_scripts: otherScripts,
};
fs.writeFileSync(path.resolve(outputArg), JSON.stringify(inventory, null, 2) + '\n');
console.log(JSON.stringify({ go_test_files: inventory.go_test_files, go_test_functions: inventory.go_test_functions,
  captured_fixture_cases: inventory.captured_fixture_cases, javascript_suites: jsSuites.map(({ file, registered_tests }) => ({ file, registered_tests })) }, null, 2));
