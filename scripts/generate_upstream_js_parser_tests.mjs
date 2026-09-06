#!/usr/bin/env node

// Capture the actual calls made by all upstream JS/TS parser and lowering tests,
// including loops and shared helpers. Run the original Go assertions as well.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

if (process.argv.length !== 4) {
  console.error('usage: node scripts/generate_upstream_js_parser_tests.mjs <upstream-root> <output-json>');
  process.exit(2);
}
const upstreamRoot = path.resolve(process.argv[2]);
const outputPath = path.resolve(process.argv[3]);
const pin = fs.readFileSync(fileURLToPath(new URL('../UPSTREAM.md', import.meta.url)), 'utf8')
  .match(/Commit: `([a-f0-9]+)`/)[1];
const revision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: upstreamRoot, encoding: 'utf8' }).trim();
if (revision !== pin) throw new Error(`Expected pinned upstream ${pin}, got ${revision}`);
const packageRoot = path.join(upstreamRoot, 'internal/js_parser');
const tempRoot = fs.mkdtempSync(path.join(upstreamRoot, 'internal/js_parser_corpus_'));
const corpusPath = path.join(tempRoot, 'corpus.json');

try {
  for (const name of fs.readdirSync(packageRoot)) {
    if (name.endsWith('.go') && name !== 'json_parser_test.go') {
      fs.copyFileSync(path.join(packageRoot, name), path.join(tempRoot, name));
    }
  }
  const testPath = path.join(tempRoot, 'js_parser_test.go');
  let source = fs.readFileSync(testPath, 'utf8');
  const originalSource = source;
  // Instrumentation adds lines above the tests. Reset Go's source positions at
  // each test so the captured locations still point into the pinned original.
  source = source.replace(/^func Test/gm, (match, offset) =>
    `//line js_parser_test.go:${originalSource.slice(0, offset).split('\n').length}\n${match}`);
  source = source.replace('import (\n', 'import (\n"encoding/json"\n"os"\n"reflect"\n"runtime"\n"strconv"\n');
  for (const [helper, kind] of [['expectParseErrorCommon', 'diagnostic'], ['expectPrintedCommon', 'print']]) {
    const signature = `func ${helper}(t *testing.T, contents string, expected string, options config.Options) {`;
    if (!source.includes(signature)) throw new Error(`Missing upstream helper ${helper}`);
    source = source.replace(signature, `${signature}\n captureJSParserCase(t, "${kind}", contents, expected, options)`);
  }
  source += `

type jsParserCorpusCase struct {
  UpstreamTest string \`json:"upstream_test"\`
  File string \`json:"file"\`
  Line int \`json:"line"\`
  Kind string \`json:"kind"\`
  Source []byte \`json:"source_base64"\`
  Expected []byte \`json:"expected_base64"\`
  Options map[string]json.RawMessage \`json:"options"\`
}
var jsParserCorpus []jsParserCorpusCase

func captureJSParserCase(t *testing.T, kind, contents, expected string, options config.Options) {
  t.Helper()
  file := ""
  line := 0
  for skip := 1; ; skip++ {
    pc, candidateFile, candidateLine, ok := runtime.Caller(skip)
    if !ok { break }
    name := runtime.FuncForPC(pc).Name()
    if strings.HasPrefix(name[strings.LastIndex(name, ".")+1:], "Test") {
      file = candidateFile[strings.LastIndex(candidateFile, "/")+1:]
      line = candidateLine
      break
    }
  }
  capturedOptions := make(map[string]json.RawMessage)
  value := reflect.ValueOf(options)
  for i := 0; i < value.NumField(); i++ {
    field := value.Field(i)
    if field.IsZero() { continue }
    name := value.Type().Field(i).Name
    data, err := json.Marshal(field.Interface())
    if name == "UnsupportedJSFeatures" || name == "UnsupportedCSSFeatures" {
      data, err = json.Marshal(strconv.FormatUint(field.Uint(), 10))
    }
    if err != nil { t.Fatalf("cannot capture option %s: %v", name, err) }
    capturedOptions[name] = data
  }
  jsParserCorpus = append(jsParserCorpus, jsParserCorpusCase{
    UpstreamTest: t.Name(), File: file, Line: line, Kind: kind,
    Source: []byte(contents), Expected: []byte(expected), Options: capturedOptions,
  })
}

func TestMain(m *testing.M) {
  code := m.Run()
  data, err := json.MarshalIndent(jsParserCorpus, "", "  ")
  if err == nil { err = os.WriteFile(os.Getenv("JS_PARSER_CORPUS_OUT"), append(data, '\\n'), 0644) }
  if err != nil { panic(err) }
  os.Exit(code)
}
`;
  fs.writeFileSync(testPath, source);
  execFileSync('gofmt', ['-w', testPath]);
  execFileSync('go', ['test', `./${path.relative(upstreamRoot, tempRoot)}`], {
    cwd: upstreamRoot, env: { ...process.env, JS_PARSER_CORPUS_OUT: corpusPath }, stdio: 'inherit',
  });
  const cases = JSON.parse(fs.readFileSync(corpusPath, 'utf8'));
  if (cases.some(item => !item.file || !item.line)) throw new Error('A captured case has no upstream location');
  fs.writeFileSync(outputPath, JSON.stringify(cases, null, 2) + '\n');
  console.log(`generated ${cases.length} upstream JS/TS parser and lowering cases`);
} finally {
  fs.rmSync(tempRoot, { recursive: true, force: true });
}
