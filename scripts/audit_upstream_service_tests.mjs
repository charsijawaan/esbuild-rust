#!/usr/bin/env node
// Execute original pinned JS API/plugin functions against the original Node
// wrapper and a chosen native binary. Only the suite's final main() invocation
// is replaced with exports. No test body, option, or assertion is rewritten.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import Module, { createRequire } from 'node:module';

const PIN = '6ff1d8b0d8c134e867a397eef39702a223ebef9e';
const [upstreamArg, binaryArg, reportArg, selection = 'vertical'] = process.argv.slice(2);
if (!upstreamArg || (!binaryArg && selection !== '--list') || (!reportArg && binaryArg !== '--list')) {
  console.error('usage: node scripts/audit_upstream_service_tests.mjs <upstream> <binary> <report.json> [minimum|vertical|core|context|callbacks|boundaries|lifecycle|all-js-api|all-plugins|group/test,...]');
  console.error('       node scripts/audit_upstream_service_tests.mjs <upstream> --list');
  process.exit(2);
}
const upstream = path.resolve(upstreamArg);
const git = (...args) => execFileSync('git', args, { cwd: upstream, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
if (git('rev-parse', 'HEAD').trim() !== PIN) throw new Error(`Expected pinned upstream ${PIN}`);
const suiteGroups = {
  'scripts/js-api-tests.js': ['buildTests', 'watchTests', 'serveTests', 'transformTests', 'formatTests', 'analyzeTests', 'apiSyncTests', 'childProcessTests', 'serialTests'],
  'scripts/plugin-tests.js': ['pluginTests', 'syncTests'],
};
const registered = [];
const hashes = {};
for (const [file, groups] of Object.entries(suiteGroups)) {
  const source = git('show', `${PIN}:${file}`);
  hashes[file] = crypto.createHash('sha256').update(source).digest('hex');
  const trailer = '\nmain().catch(e => setTimeout(() => { throw e }))\n';
  if (!source.endsWith(trailer)) throw new Error(`Unknown suite entry point: ${file}`);
  const filename = path.join(upstream, file);
  const mod = new Module(filename);
  mod.filename = filename;
  mod.paths = Module._nodeModulePaths(path.dirname(filename));
  mod.require = createRequire(filename);
  mod._compile(source.slice(0, -trailer.length) + `\nmodule.exports = { ${groups.join(', ')} };\n`, filename);
  for (const [group, functions] of Object.entries(mod.exports)) {
    for (const [name, fn] of Object.entries(functions)) {
      if (typeof fn !== 'function') throw new Error(`Non-function registration: ${group}/${name}`);
      const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      const match = new RegExp(`^  (?:async )?${escaped}\\(`, 'm').exec(source);
      registered.push({ file, group, name, id: `${group}/${name}`, fn,
        line: match ? source.slice(0, match.index).split('\n').length : null });
    }
  }
}
if (binaryArg === '--list') {
  console.log(JSON.stringify(registered.map(({ fn, ...item }) => item), null, 2));
  process.exit(0);
}

const core = [
  'transformTests/ignoreUndefinedOptions', 'transformTests/throwOnBadOptions',
  'transformTests/transformLoaderBase64', 'transformTests/jsCharsetUTF8',
  'transformTests/mangleCacheTransform', 'transformTests/transformLegalCommentsJS',
  'transformTests/sourceMapExternalWithName', 'apiSyncTests/transformThrow',
  'buildTests/errorIfEntryPointsNotArray', 'buildTests/errorIfBadWorkingDirectory',
  'buildTests/mangleCacheBuild', 'buildTests/writeFalse', 'buildTests/workingDirTest',
  'buildTests/customEntryPointOutputPathsRel', 'buildTests/sourceMapTrue',
  'apiSyncTests/buildSync', 'apiSyncTests/buildSyncOutputFiles',
  'apiSyncTests/transformSyncJSMap', 'apiSyncTests/transformSyncCSS',
  'formatTests/formatMessages', 'analyzeTests/analyzeMetafile', 'serialTests/startStop',
];
const context = [
  'buildTests/rebuildBasic', 'buildTests/rebuildIndependent',
  'buildTests/rebuildParallel', 'buildTests/rebuildMerging',
];
const callbacks = [
  'pluginTests/modifyInitialOptionsAsync', 'pluginTests/basicLoader',
  'pluginTests/basicResolver', 'pluginTests/resolversCalledInSequence',
  'pluginTests/loadersCalledInSequence', 'pluginTests/pluginDataResolveToLoad',
  'pluginTests/specificDetailForOnResolvePluginReturnError',
  'pluginTests/callResolveBuiltInHandler', 'pluginTests/callResolvePluginHandler',
  'pluginTests/callResolveTooLateError', 'pluginTests/invalidRegExp',
  'syncTests/onStartCallbackWithDelay', 'syncTests/onEndCallback',
  'syncTests/onEndCallbackMutateContents', 'syncTests/pluginOnDisposeWithUnusedContext',
  'syncTests/pluginOnDisposeWithRebuild',
];
const boundaries = [
  'buildTests/buildLoaderStdinBase64', 'buildTests/rebuildCancel',
  'buildTests/rapidRebuildCancel', 'watchTests/watchWriteFalse',
  'watchTests/watchTwice', 'serveTests/serveBasic',
];
const minimum = [
  'serialTests/startStop', 'apiSyncTests/transformThrow', 'transformTests/throwOnBadOptions',
  'transformTests/transformLoaderBase64', 'buildTests/writeFalse', 'buildTests/workingDirTest',
  'buildTests/rebuildBasic', 'buildTests/rebuildMerging', 'syncTests/onStartCallbackWithDelay',
  'syncTests/onEndCallback', 'pluginTests/callResolveBuiltInHandler',
  'pluginTests/callResolveTooLateError', 'syncTests/pluginOnDisposeWithUnusedContext',
];
const lifecycle = ['serialTests/startStop', 'childProcessTests/testIncrementalChildProcessExit',
  'syncTests/pluginOnDisposeAfterSuccessfulBuild', 'syncTests/pluginOnDisposeAfterFailedBuild'];
const presets = { minimum, core, context, callbacks, boundaries, lifecycle, vertical: [...core, ...context, ...callbacks] };
const ids = selection === 'all-js-api' ? registered.filter(t => t.file.endsWith('js-api-tests.js')).map(t => t.id)
  : selection === 'all-plugins' ? registered.filter(t => t.file.endsWith('plugin-tests.js')).map(t => t.id)
  : presets[selection] || selection.split(',');
const tests = ids.map(id => {
  const item = registered.find(t => t.id === id);
  if (!item) throw new Error(`Unknown original test: ${id}`);
  return item;
});

const binary = path.resolve(binaryArg);
const reportPath = path.resolve(reportArg);
const goBinary = path.resolve(process.env.ESBUILD_TEAM_WRAPPER_COMPILER || path.join(upstream, 'esbuild'));
const version = git('show', `${PIN}:version.txt`).trim();
execFileSync('git', ['diff', '--exit-code', PIN, '--', 'lib'], { cwd: upstream, stdio: 'pipe' });
if (execFileSync(goBinary, ['--version'], { encoding: 'utf8' }).trim() !== version) {
  throw new Error(`Wrapper compiler must be pinned Go esbuild ${version}`);
}
// Keep the published wrapper's exact basename/layout for worker-thread support.
const artifactRoot = fs.realpathSync(fs.mkdtempSync('/tmp/esbuild-service-original-'));
const packageDir = path.join(artifactRoot, 'package');
const wrapper = path.join(packageDir, 'lib', 'main.js');
fs.mkdirSync(path.dirname(wrapper), { recursive: true });
// This is exactly the buildNeutralLib() command for main.js in scripts/esbuild.js.
execFileSync(goBinary, [path.join(upstream, 'lib', 'npm', 'node.ts'),
  `--outfile=${wrapper}`, '--bundle', '--target=node10', '--define:WASM=false',
  `--define:ESBUILD_VERSION=${JSON.stringify(version)}`, '--external:esbuild',
  '--platform=node', '--log-level=warning'], { cwd: upstream, stdio: 'inherit' });
const shim = path.join(packageDir, 'bin', 'esbuild');
fs.mkdirSync(path.dirname(shim), { recursive: true });
execFileSync(goBinary, [path.join(upstream, 'lib', 'npm', 'node-shim.ts'),
  `--outfile=${shim}`, '--bundle', '--target=node10',
  `--define:ESBUILD_VERSION=${JSON.stringify(version)}`, '--external:esbuild',
  '--platform=node', '--log-level=warning'], { cwd: upstream, stdio: 'inherit' });
fs.chmodSync(shim, 0o755);
fs.writeFileSync(path.join(packageDir, 'package.json'), JSON.stringify({ name: 'esbuild', version, main: 'lib/main.js' }));
// Preserve original relative test paths because some original assertions hash
// output that includes a source-path comment. All test writes stay under /tmp.
const sandbox = path.join(artifactRoot, 'sandbox');
fs.mkdirSync(sandbox);
process.chdir(sandbox);
process.env.ESBUILD_BINARY_PATH = binary;
const require = createRequire(wrapper);
const esbuild = require(wrapper);
Object.defineProperty(esbuild, 'ESBUILD_PACKAGE_PATH', { value: packageDir });

const report = { upstream_revision: PIN, upstream_version: version, suite_sha256: hashes,
  wrapper: { source: 'lib/npm/node.ts', compiler: goBinary, path: wrapper },
  binary, binary_sha256: crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex'),
  node_version: process.version, platform: process.platform, architecture: process.arch,
  worker_threads: process.env.ESBUILD_WORKER_THREADS !== '0', selection, artifact_root: artifactRoot,
  execution: 'original-functions; sequential test scheduling; original internal concurrency retained',
  results: [], completed: false };
const save = () => fs.writeFileSync(reportPath, JSON.stringify(report, null, 2) + '\n');
save();
const timeoutMs = Number(process.env.ESBUILD_TEAM_SERVICE_TIMEOUT_MS || 30000);
if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1) throw new Error('Invalid test timeout');
try {
  for (const { fn, ...test } of tests) {
    const testRoot = test.file.endsWith('js-api-tests.js') ? '.js-api-tests' : '.plugin-tests';
    const testDir = path.join(sandbox, 'scripts', testRoot, test.name);
    fs.mkdirSync(testDir, { recursive: true });
    const start = performance.now();
    const timeout = setTimeout(() => {
      report.results.push({ ...test, passed: false, timed_out: true, elapsed_ms: performance.now() - start, test_dir: testDir });
      save();
      esbuild.stop();
      console.error(`TIMEOUT ${test.id}`);
      process.exit(1);
    }, timeoutMs);
    try {
      await fn({ esbuild, testDir });
      report.results.push({ ...test, passed: true, elapsed_ms: performance.now() - start });
      fs.rmSync(testDir, { recursive: true, force: true });
      console.log(`PASS ${test.id}`);
    } catch (error) {
      report.results.push({ ...test, passed: false, elapsed_ms: performance.now() - start,
        error: error?.stack || String(error), test_dir: testDir });
      console.error(`FAIL ${test.id}: ${error?.message || error}`);
    } finally {
      clearTimeout(timeout);
      save();
    }
  }
  report.completed = true;
  report.passed = report.results.filter(t => t.passed).length;
  report.failed = report.results.length - report.passed;
  save();
  console.log(JSON.stringify({ passed: report.passed, failed: report.failed, report: reportPath, artifact_root: artifactRoot }));
  process.exitCode = report.failed ? 1 : 0;
} finally {
  esbuild.stop();
}

