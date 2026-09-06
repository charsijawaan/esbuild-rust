#!/usr/bin/env node

// Execute the pinned upstream CLI/runtime tests with their original assertions.
// Each registered test gets a process group so a bad generated program cannot
// hang the audit or leave a watch process running indefinitely.
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Module, { createRequire } from 'node:module';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const scriptPath = fileURLToPath(import.meta.url);
const [upstreamArg, binaryArg, reportArg] = process.argv.slice(2);
if (!upstreamArg || !binaryArg || !reportArg) {
  console.error('usage: node scripts/audit_upstream_end_to_end_tests.mjs <pinned-upstream-root> <binary> <report.json>');
  process.exit(2);
}
const upstreamRoot = path.resolve(upstreamArg);
const binary = path.resolve(binaryArg);
const reportPath = path.resolve(reportArg);
const pin = fs.readFileSync(new URL('../UPSTREAM.md', import.meta.url), 'utf8')
  .match(/Commit: `([a-f0-9]+)`/)[1];
const revision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: upstreamRoot, encoding: 'utf8' }).trim();
if (revision !== pin) throw new Error(`Expected pinned upstream ${pin}, got ${revision}`);
const upstreamFile = path.join(upstreamRoot, 'scripts/end-to-end-tests.js');
// Read the committed file, not potentially modified contents in the checkout.
const original = execFileSync('git', ['show', `${pin}:scripts/end-to-end-tests.js`], {
  cwd: upstreamRoot, encoding: 'utf8', maxBuffer: 8 * 1024 * 1024,
});

function loadTests(scratchRoot) {
  const require = createRequire(upstreamFile);
  const helpers = require('./esbuild.js');
  let source = original;
  const mainCall = '\nmain()\n';
  if (!source.endsWith(mainCall)) throw new Error('Upstream entry point changed');
  source = source.slice(0, -mainCall.length) + '\nmodule.exports = { tests, testDir };\n';
  // Wrap registration helpers without inserting lines, preserving source locations.
  const registration = ['test', 'testStdout', 'testWatch', 'testWatchStdout'].map(name =>
    `const original_${name} = ${name}; ${name} = (...args) => { const fn = original_${name}(...args); ` +
    `fn.upstream = { kind: ${JSON.stringify(name)}, args: ${name === 'test' ? 'args[0]' : name === 'testStdout' ? 'args[1]' : 'null'}, ` +
    `location: new Error().stack.split('\\n')[2].trim() }; return fn; };`
  ).join(' ');
  if (!source.includes('let testCount = 0\n')) throw new Error('Upstream registration changed');
  source = source.replace('let testCount = 0\n', `let testCount = 0; ${registration}\n`);
  const module = new Module(upstreamFile);
  module.filename = upstreamFile;
  module.require = name => name === './esbuild.js'
    ? { ...helpers, dirname: scratchRoot, buildBinary: () => binary }
    : require(name);
  module._compile(source, upstreamFile);
  if (module.exports.tests.some(test => !test.upstream)) throw new Error('Untracked upstream test');
  return module.exports;
}

if (process.env.ESBUILD_RS_E2E_WORKER_INDEX !== undefined) {
  const index = Number(process.env.ESBUILD_RS_E2E_WORKER_INDEX);
  const suite = loadTests(process.env.ESBUILD_RS_E2E_WORKER_ROOT);
  fs.mkdirSync(suite.testDir, { recursive: true });
  try {
    const passed = await suite.tests[index]();
    console.log('UPSTREAM_E2E_RESULT\t' + JSON.stringify({ index, passed: passed === true }));
    process.exit(passed === true ? 0 : 1);
  } catch (error) {
    console.error(error.stack || error);
    process.exit(1);
  }
}

fs.accessSync(binary, fs.constants.X_OK);
console.log(`Checking executable startup: ${binary}`);
execFileSync(binary, ['--version'], { timeout: 180000, stdio: 'pipe' });
const binaryHash = createHash('sha256').update(fs.readFileSync(binary)).digest('hex');
// macOS exposes /var through a symlink to /private/var. Use the canonical root
// so upstream's absolute-path diagnostic assertions see the same spelling as cwd.
const scratchRoot = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'esbuild-rust-upstream-e2e-')));
const { tests } = loadTests(scratchRoot);
const start = Number(process.env.ESBUILD_RS_E2E_START || 0);
const limit = Number(process.env.ESBUILD_RS_E2E_LIMIT || tests.length);
const timeoutMs = Number(process.env.ESBUILD_RS_E2E_TIMEOUT_MS || 90000);
if (![start, limit, timeoutMs].every(Number.isSafeInteger) || start < 0 || limit < 1 || timeoutMs < 1) {
  throw new Error('Invalid E2E selection or timeout');
}
const indices = tests.map((_, i) => i).slice(start, start + limit);
if (!indices.length) throw new Error('No upstream end-to-end tests selected');
const children = new Set();
function killGroup(child, signal) {
  if (!child.pid) return;
  try {
    if (process.platform === 'win32') child.kill(signal);
    else process.kill(-child.pid, signal);
  } catch (error) {
    if (error.code !== 'ESRCH') throw error;
  }
}
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => {
  for (const child of children) killGroup(child, 'SIGKILL');
  process.exit(signal === 'SIGINT' ? 130 : 143);
});
const results = [];
function saveReport() {
  fs.writeFileSync(reportPath, JSON.stringify({
    upstream_revision: pin, binary, scratch_root: scratchRoot,
    binary_sha256: binaryHash,
    node_version: process.version, platform: process.platform, architecture: process.arch,
    registered_tests: tests.length, selected_tests: indices.length,
    complete: results.length === indices.length,
    // A registered test may exercise both CJS and ESM. Do not conflate this
    // count with captured parser helper calls or top-level Go test functions.
    results: results.slice().sort((a, b) => a.index - b.index),
  }, null, 2) + '\n');
}
async function run(index) {
  const workerRoot = path.join(scratchRoot, String(index));
  fs.mkdirSync(workerRoot);
  return await new Promise(resolve => {
    const child = spawn(process.execPath, [scriptPath, upstreamRoot, binary, reportPath], {
      env: { ...process.env, ESBUILD_RS_E2E_WORKER_INDEX: String(index), ESBUILD_RS_E2E_WORKER_ROOT: workerRoot },
      detached: process.platform !== 'win32', stdio: ['ignore', 'pipe', 'pipe'],
    });
    children.add(child);
    let output = '';
    let timedOut = false;
    const append = chunk => { output = (output + chunk).slice(-16000); };
    child.stdout.on('data', append);
    child.stderr.on('data', append);
    const timer = setTimeout(() => {
      timedOut = true;
      killGroup(child, 'SIGKILL');
    }, timeoutMs);
    child.on('error', error => append(error.stack || error));
    child.on('close', code => {
      clearTimeout(timer);
      killGroup(child, 'SIGKILL');
      children.delete(child);
      const passed = code === 0 && output.includes(`UPSTREAM_E2E_RESULT\t${JSON.stringify({ index, passed: true })}`);
      resolve({ index, ...tests[index].upstream, status: timedOut ? 'timeout' : passed ? 'passed' : 'failed',
        ...(passed ? {} : { output, directory: workerRoot }) });
    });
  });
}
console.log(`Running ${indices.length} of ${tests.length} registered upstream end-to-end tests`);
saveReport();
let cursor = 0;
await Promise.all(Array.from({ length: 4 }, async () => {
  while (cursor < indices.length) {
    const result = await run(indices[cursor++]);
    results.push(result);
    saveReport();
    if (results.length % 25 === 0 || results.length === indices.length) {
      console.log(`${results.length}/${indices.length}: ${results.filter(x => x.status === 'passed').length} passed`);
    }
  }
}));
console.log(`Report: ${reportPath}`);
if (results.every(result => result.status === 'passed')) {
  fs.rmSync(scratchRoot, { recursive: true });
} else {
  console.log(`Failing test artifacts: ${scratchRoot}`);
  process.exitCode = 1;
}
