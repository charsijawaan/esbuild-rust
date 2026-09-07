#!/usr/bin/env node

// Capture every helper call from both pinned Go API test files while executing
// their original assertions. Do not substitute the port's output as an oracle.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

if (process.argv.length !== 4) {
  console.error('usage: node scripts/generate_upstream_api_tests.mjs <upstream-root> <output-json>');
  process.exit(2);
}
const upstreamRoot = path.resolve(process.argv[2]);
const outputPath = path.resolve(process.argv[3]);
const pin = fs.readFileSync(new URL('../UPSTREAM.md', import.meta.url), 'utf8')
  .match(/Commit: `([a-f0-9]+)`/)[1];
const git = (...args) => execFileSync('git', args, { cwd: upstreamRoot, encoding: 'utf8' });
if (git('rev-parse', 'HEAD').trim() !== pin) throw new Error(`Expected pinned upstream ${pin}`);
const files = git('ls-tree', '-r', '--name-only', pin, 'pkg/api').trim().split('\n')
  .filter(file => file.endsWith('.go'));
const testFiles = files.filter(file => file.endsWith('_test.go')).sort();
if (JSON.stringify(testFiles) !== JSON.stringify(['pkg/api/api_impl_test.go', 'pkg/api/api_test.go'])) {
  throw new Error('Upstream API test inventory changed');
}
const scratch = fs.mkdtempSync(path.join(upstreamRoot, 'pkg/api_corpus_'));
const corpusPath = path.join(scratch, 'corpus.jsonl');
try {
  for (const file of files) fs.writeFileSync(path.join(scratch, path.basename(file)), git('show', `${pin}:${file}`));
  for (const name of ['api_impl_test.go', 'api_test.go']) {
    const target = path.join(scratch, name);
    const original = fs.readFileSync(target, 'utf8');
    const tests = [...original.matchAll(/^func (Test\w+)\(/gm)].map(match => match[1]);
    const expectedTest = name === 'api_test.go' ? 'TestFormatMessages' : 'TestStripDirPrefix';
    if (JSON.stringify(tests) !== JSON.stringify([expectedTest])) {
      throw new Error(`Unmapped upstream tests in ${name}`);
    }
    let source = original.replace(/^func Test/gm, (match, offset) =>
      `//line ${name}:${original.slice(0, offset).split('\n').length}\n${match}`);
    source = source.replace('import (\n', 'import (\n"encoding/json"\n"os"\n"path/filepath"\n"runtime"\n');
    const injections = name === 'api_test.go' ? [[
      'check := func(name string, opts api.FormatMessagesOptions, msg api.Message, expected string) {',
      'captureAPICase(t, map[string]interface{}{"kind": "format", "name": name, "options": opts, "message": msg, "expected": expected})',
    ]] : [[
      'expectSuccess := func(path string, prefix string, allowedSlashes string, expected string) {',
      'captureAPICase(t, map[string]interface{}{"kind": "strip_dir_prefix", "path": path, "prefix": prefix, "allowed_slashes": allowedSlashes, "expected": expected, "success": true})',
    ], [
      'expectFailure := func(path string, prefix string, allowedSlashes string) {',
      'captureAPICase(t, map[string]interface{}{"kind": "strip_dir_prefix", "path": path, "prefix": prefix, "allowed_slashes": allowedSlashes, "success": false})',
    ]];
    for (const [signature, capture] of injections) {
      if (!source.includes(signature)) throw new Error(`Missing helper in ${name}: ${signature}`);
      const nextLine = original.slice(0, original.indexOf(signature)).split('\n').length + 1;
      // Reset positions after instrumentation inside each test's local helper.
      source = source.replace(`${signature}\n`, `${signature}\n${capture}\n//line ${name}:${nextLine}\n`);
    }
    source += `
func captureAPICase(t *testing.T, record map[string]interface{}) {
  t.Helper()
  _, file, line, ok := runtime.Caller(2)
  if !ok { t.Fatal("missing upstream call site") }
  record["file"] = filepath.Base(file)
  record["line"] = line
  record["upstream_test"] = t.Name()
  data, err := json.Marshal(record)
  if err != nil { t.Fatal(err) }
  output, err := os.OpenFile(os.Getenv("API_CORPUS_OUT"), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
  if err != nil { t.Fatal(err) }
  _, err = output.Write(append(data, '\\n'))
  closeErr := output.Close()
  if err != nil { t.Fatal(err) }
  if closeErr != nil { t.Fatal(closeErr) }
}
`;
    fs.writeFileSync(target, source);
    execFileSync('gofmt', ['-w', target]);
  }
  execFileSync('go', ['test', `./${path.relative(upstreamRoot, scratch)}`], {
    cwd: upstreamRoot, env: { ...process.env, API_CORPUS_OUT: corpusPath }, stdio: 'inherit',
  });
  const cases = fs.readFileSync(corpusPath, 'utf8').trim().split('\n').map(line => JSON.parse(line));
  if (cases.some(item => !item.file || !item.line)) throw new Error('Missing source location');
  fs.writeFileSync(outputPath, JSON.stringify(cases, null, 2) + '\n');
  console.log(`generated ${cases.length} upstream Go API cases`);
} finally {
  fs.rmSync(scratch, { recursive: true, force: true });
}
