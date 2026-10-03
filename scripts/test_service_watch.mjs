#!/usr/bin/env node
// Focused service watch probes using a pinned, unchanged Node API wrapper.
// Usage: node scripts/test_service_watch.mjs <binary> <original-report.json> <report.json>
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const [binaryArg, referenceArg, reportArg] = process.argv.slice(2);
if (!binaryArg || !referenceArg || !reportArg) throw new Error('Expected binary, original wrapper report, and output report');
const reference = JSON.parse(fs.readFileSync(referenceArg, 'utf8'));
assert.equal(reference.upstream_revision, '6ff1d8b0d8c134e867a397eef39702a223ebef9e');
const binary = path.resolve(binaryArg);
process.env.ESBUILD_BINARY_PATH = binary;
const wrapper = reference.wrapper.path;
const esbuild = createRequire(wrapper)(wrapper);
const root = fs.realpathSync(fs.mkdtempSync('/tmp/esbuild-service-watch-probes-'));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const deferred = () => {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
};
const bounded = (promise, label) => new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error(`Timed out: ${label}`)), 8000);
  Promise.resolve(promise).then(resolve, reject).finally(() => clearTimeout(timer));
});
async function until(test, label) {
  const deadline = Date.now() + 8000;
  while (!test()) {
    if (Date.now() >= deadline) throw new Error(`Timed out: ${label}`);
    await delay(20);
  }
}
function events() {
  const queued = [];
  const waiting = [];
  return {
    push(value) { waiting.length ? waiting.shift()(value) : queued.push(value); },
    next() { return bounded(queued.length ? queued.shift() : new Promise(resolve => waiting.push(resolve)), 'watch onEnd'); },
  };
}
function disk(name) {
  const dir = path.join(root, name);
  fs.mkdirSync(dir);
  const input = path.join(dir, 'in.js');
  const outfile = path.join(dir, 'out.js');
  fs.writeFileSync(input, 'throw 1');
  return { input, outfile };
}
function edit(input, contents) {
  fs.writeFileSync(`${input}.new`, contents);
  fs.renameSync(`${input}.new`, input);
}
const contents = result => result.outputFiles[0].text;
const options = ({ input, outfile }, plugins = []) => ({
  entryPoints: [input], outfile, format: 'esm', write: false, logLevel: 'silent', plugins,
});

async function realDiskChangesAndRecovery() {
  const files = disk('disk-recovery');
  const end = events();
  const ctx = await esbuild.context(options(files, [{ name: 'observe', setup(build) {
    build.onEnd(result => { end.push(result); });
  } }]));
  try {
    await ctx.watch();
    assert.equal(contents(await end.next()), 'throw 1;\n');
    assert.equal(fs.existsSync(files.outfile), false);
    edit(files.input, 'throw 2');
    assert.equal(contents(await end.next()), 'throw 2;\n');
    edit(files.input, 'throw 1 2');
    const failed = await end.next();
    assert.equal(failed.errors.length, 1);
    assert.equal(failed.errors[0].text, 'Expected ";" but found "2"');
    edit(files.input, 'throw 3');
    const recovered = await end.next();
    assert.equal(recovered.errors.length, 0);
    assert.equal(contents(recovered), 'throw 3;\n');
    fs.unlinkSync(files.input);
    assert.equal((await end.next()).errors.length, 1);
    edit(files.input, 'throw 4');
    assert.equal(contents(await end.next()), 'throw 4;\n');
    assert.equal(fs.existsSync(files.outfile), false);
  } finally { await ctx.dispose(); }
}

async function backgroundWithoutObserverAndManualRebuild() {
  const files = disk('unobserved');
  const ctx = await esbuild.context({ ...options(files), write: true });
  try {
    await ctx.watch();
    await until(() => fs.existsSync(files.outfile), 'unobserved initial build');
    assert.equal(fs.readFileSync(files.outfile, 'utf8'), 'throw 1;\n');
    edit(files.input, 'throw 2');
    await until(() => fs.readFileSync(files.outfile, 'utf8') === 'throw 2;\n', 'unobserved edit');
    const rebuilt = await bounded(ctx.rebuild(), 'manual rebuild after background build');
    assert.equal(rebuilt.errors.length, 0);
    await ctx.dispose();
    edit(files.input, 'throw 3');
    await delay(350);
    assert.equal(fs.readFileSync(files.outfile, 'utf8'), 'throw 2;\n');
  } finally { await ctx.dispose(); }
}

async function coalescingAndAcknowledgement() {
  const files = disk('coalescing');
  const entered = deferred();
  const release = deferred();
  const end = events();
  let runs = 0;
  const ctx = await esbuild.context(options(files, [{ name: 'hold-end', setup(build) {
    build.onEnd(async result => {
      runs++;
      end.push(result);
      if (runs === 1) { entered.resolve(); await release.promise; }
    });
  } }]));
  try {
    await ctx.watch();
    await bounded(entered.promise, 'initial callback entered');
    let finished = false;
    const a = ctx.rebuild();
    const b = ctx.rebuild();
    a.then(() => { finished = true; });
    await esbuild.transform('let fence = 1');
    await delay(100);
    assert.equal(finished, false, 'manual rebuild waits for host onEnd acknowledgement');
    assert.equal(runs, 1, 'manual rebuild joins the active native watch build');
    release.resolve();
    const [first, second] = await bounded(Promise.all([a, b]), 'merged rebuilds');
    assert.equal(first, second);
    assert.equal(contents(first), 'throw 1;\n');
    await end.next();
    edit(files.input, 'throw 2');
    assert.equal(contents(await end.next()), 'throw 2;\n');
    assert.equal(runs, 2);
  } finally { release.resolve(); await ctx.dispose(); }
}

async function disposeCallbackLifetime(stage) {
  const files = disk(`dispose-${stage}`);
  const entered = deferred();
  const release = deferred();
  const cleanupStarted = deferred();
  const cleanupRelease = deferred();
  const cleanupDone = deferred();
  let resolve;
  let disposed = false;
  let cleanupFinished = false;
  const ctx = await esbuild.context(options(files, [{ name: 'lifetime', setup(build) {
    resolve = build.resolve;
    build.onResolve({ filter: /^late$/ }, () => ({ path: 'late', external: true }));
    build[stage === 'on-start' ? 'onStart' : 'onEnd'](async () => {
      entered.resolve(); await release.promise;
    });
    build.onDispose(async () => {
      cleanupStarted.resolve(); await cleanupRelease.promise;
      cleanupFinished = true; cleanupDone.resolve();
    });
  } }]));
  try {
    await ctx.watch();
    await bounded(entered.promise, 'paused background callback');
    const disposing = ctx.dispose().then(() => { disposed = true; });
    await esbuild.transform('let fence = 1');
    await delay(100);
    assert.equal(disposed, false, 'dispose waits for the active native background build');
    const late = await bounded(resolve('late', { kind: 'import-statement' }), 'resolve during disposal');
    assert.equal(late.errors.length, 0);
    assert.equal(late.external, true);
    release.resolve();
    await bounded(disposing, 'disposal after callback acknowledgement');
    await bounded(cleanupStarted.promise, 'asynchronous onDispose start');
    assert.equal(disposed, true);
    assert.equal(cleanupFinished, false, 'dispose does not await the JavaScript onDispose promise');
    cleanupRelease.resolve();
    await bounded(cleanupDone.promise, 'asynchronous onDispose finish');
    await assert.rejects(resolve('late', { kind: 'import-statement' }), /inactive build/);
  } finally { release.resolve(); cleanupRelease.resolve(); await ctx.dispose(); }
}

async function callbackErrorRecovery() {
  const files = disk('host-error');
  const end = events();
  let runs = 0;
  const ctx = await esbuild.context(options(files, [{ name: 'host-errors', setup(build) {
    build.onEnd(result => {
      end.push(result);
      if (++runs === 1) return { errors: [{ text: 'host callback error' }] };
    });
  } }]));
  try {
    await ctx.watch();
    const failed = await end.next();
    await esbuild.transform('let fence = 1');
    assert.equal(failed.errors[0].text, 'host callback error');
    edit(files.input, 'throw 2');
    const recovered = await end.next();
    assert.equal(recovered.errors.length, 0);
    assert.equal(contents(recovered), 'throw 2;\n');
  } finally { await ctx.dispose(); }
}

const report = {
  upstream_revision: reference.upstream_revision,
  binary, binary_sha256: crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex'),
  wrapper, wrapper_sha256: crypto.createHash('sha256').update(fs.readFileSync(wrapper)).digest('hex'),
  worker_threads: process.env.ESBUILD_WORKER_THREADS !== '0', artifact_root: root,
  completed: false, results: [],
};
const save = () => fs.writeFileSync(reportArg, JSON.stringify(report, null, 2) + '\n');
const tests = {
  realDiskChangesAndRecovery,
  backgroundWithoutObserverAndManualRebuild,
  coalescingAndAcknowledgement,
  disposeOnStartLifetime: () => disposeCallbackLifetime('on-start'),
  disposeOnEndLifetime: () => disposeCallbackLifetime('on-end'),
  callbackErrorRecovery,
};
save();
try {
  for (const [name, run] of Object.entries(tests)) {
    const started = performance.now();
    try {
      await bounded(run(), name);
      report.results.push({ name, passed: true, elapsed_ms: performance.now() - started });
      console.log(`PASS ${name}`);
    } catch (error) {
      report.results.push({ name, passed: false, error: error.stack, elapsed_ms: performance.now() - started });
      console.error(`FAIL ${name}: ${error.stack}`);
    }
    save();
  }
  report.completed = true;
  report.passed = report.results.filter(result => result.passed).length;
  report.failed = report.results.length - report.passed;
  save();
  process.exitCode = report.failed ? 1 : 0;
} finally { esbuild.stop(); }
