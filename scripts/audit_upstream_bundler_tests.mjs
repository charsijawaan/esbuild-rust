// Audit inactive fixtures without changing their expected output or active status.
// Usage: node scripts/audit_upstream_bundler_tests.mjs [report.json] [suite]
import { spawn, spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const test = 'internal::bundler::tests::matches_pinned_upstream_active_bundler_corpus';
const reportPath = process.argv[2];
const suiteFilter = process.argv[3];
const build = spawnSync('cargo', ['test', '--lib', '--no-run', '--message-format=json'], {
  cwd: root, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024,
});
if (build.status !== 0) {
  process.stderr.write(build.stderr || build.stdout);
  process.exit(build.status || 1);
}
const artifact = build.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line))
  .find(item => item.reason === 'compiler-artifact' && item.executable && item.profile.test);
if (!artifact) throw new Error('Cargo did not report the library test executable');

const baseEnv = { ...process.env };
delete baseEnv.ESBUILD_RS_UPSTREAM_TEST;
delete baseEnv.ESBUILD_RS_UPSTREAM_LIST;
const listing = spawnSync(artifact.executable, ['--exact', test, '--nocapture'], {
  cwd: root, env: { ...baseEnv, ESBUILD_RS_UPSTREAM_LIST: '1' },
  encoding: 'utf8', maxBuffer: 4 * 1024 * 1024,
});
if (listing.status !== 0) throw new Error(listing.stderr || listing.stdout);
const corpus = JSON.parse(readFileSync(new URL('../tests/upstream/bundler.json', import.meta.url)));
const inactive = listing.stdout.split('\n').filter(line => line.startsWith('INACTIVE\t'))
  .map(line => {
    const [, suite, name, fileSystem] = line.split('\t');
    const fixture = corpus.find(item => item.upstream_test === name && item.suite === suite && item.file_system === fileSystem);
    return { suite, name, fileSystem, fixture };
  }).filter(item => !suiteFilter || item.suite === suiteFilter);
const results = [];
let cursor = 0;

async function worker() {
  while (cursor < inactive.length) {
    const item = inactive[cursor++];
    let result;
    if (item.fileSystem !== 'unix') {
      result = { status: 'unsupported-filesystem' };
    } else if (item.name === 'TestGlobDirDoesNotExist') {
      result = { status: 'covered-separately' };
    } else if (item.fixture.unsupported_options?.length) {
      result = { status: 'unsupported-options', options: item.fixture.unsupported_options };
    } else {
      result = await new Promise(resolve => {
        const child = spawn(artifact.executable, ['--exact', test, '--nocapture'], {
          cwd: root, env: { ...baseEnv, ESBUILD_RS_UPSTREAM_TEST: item.name },
        });
        let output = '';
        let timedOut = false;
        const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL'); }, 180_000);
        child.stdout.on('data', data => { output += data; });
        child.stderr.on('data', data => { output += data; });
        child.on('error', error => { clearTimeout(timer); resolve({ status: 'execution-error', output: String(error) }); });
        child.on('close', code => {
          clearTimeout(timer);
          resolve({ status: timedOut ? 'timeout' : code === 0 ? 'passing-candidate' : 'failed',
            ...(code !== 0 ? { output } : {}) });
        });
      });
    }
    results.push({ suite: item.suite, name: item.name, fileSystem: item.fileSystem, ...result });
    process.stderr.write(`[${results.length}/${inactive.length}] ${result.status}: ${item.name}\n`);
  }
}

await Promise.all(Array.from({ length: 4 }, worker));
results.sort((a, b) => a.suite.localeCompare(b.suite) || a.name.localeCompare(b.name));
const counts = {};
for (const result of results) counts[result.status] = (counts[result.status] || 0) + 1;
const report = { counts, results };
if (reportPath) writeFileSync(reportPath, JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify(counts, null, 2));
