#!/usr/bin/env node

// Execute an instrumented temporary copy of the pinned upstream bundler tests.
// Capturing calls to expectBundled at runtime includes cases assembled by loops
// and helper functions that a source-only extractor would silently miss.

import childProcess from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

if (process.argv.length !== 4) {
  console.error(
    "usage: node scripts/generate_upstream_bundler_tests.mjs <upstream-root> <output-json>",
  );
  process.exit(2);
}

const upstreamRoot = path.resolve(process.argv[2]);
const outputPath = path.resolve(process.argv[3]);
const packageRoot = path.join(upstreamRoot, "internal", "bundler_tests");
const tempRoot = fs.mkdtempSync(
  path.join(upstreamRoot, "internal", "bundler_tests_corpus_"),
);
const corpusPath = path.join(tempRoot, "corpus.json");

try {
  for (const name of fs.readdirSync(packageRoot)) {
    if (name.endsWith(".go")) {
      fs.copyFileSync(path.join(packageRoot, name), path.join(tempRoot, name));
    }
  }

  const testPath = path.join(tempRoot, "bundler_test.go");
  let source = fs.readFileSync(testPath, "utf8");
  source = source.replace(
    'import (\n',
    'import (\n\t"encoding/json"\n\t"reflect"\n\t"runtime"\n',
  );

  source = replaceFunctionBody(
    source,
    "func (s *suite) expectBundled(t *testing.T, args bundled) ",
    "{ captureBundledCase(t, s, args, \"unix\") }",
  );
  source = replaceFunctionBody(
    source,
    "func (s *suite) expectBundledUnix(t *testing.T, args bundled) ",
    "{ captureBundledCase(t, s, args, \"unix\") }",
  );
  source = replaceFunctionBody(
    source,
    "func (s *suite) expectBundledWindows(t *testing.T, args bundled) ",
    "{ captureBundledCase(t, s, args, \"windows\") }",
  );
  source = replaceFunctionBody(
    source,
    "func TestMain(m *testing.M) ",
    `{
\tcode := m.Run()
\tdata, err := json.MarshalIndent(bundlerCorpus, "", "  ")
\tif err == nil { err = os.WriteFile(os.Getenv("BUNDLER_CORPUS_OUT"), append(data, '\\n'), 0644) }
\tif err != nil { panic(err) }
\tos.Exit(code)
}`,
  );

  source += String.raw`

type bundlerCorpusCase struct {
	UpstreamTest string                     ` + "`json:\"upstream_test\"`" + String.raw`
	Line int                                ` + "`json:\"line\"`" + String.raw`
	Suite string                            ` + "`json:\"suite\"`" + String.raw`
	FileSystem string                       ` + "`json:\"file_system\"`" + String.raw`
	Files map[string]string                 ` + "`json:\"files\"`" + String.raw`
	EntryPaths []string                     ` + "`json:\"entry_paths\"`" + String.raw`
	EntryPathsAdvanced []bundler.EntryPoint ` + "`json:\"entry_paths_advanced\"`" + String.raw`
	ExpectedScanLog string                  ` + "`json:\"expected_scan_log\"`" + String.raw`
	ExpectedCompileLog string               ` + "`json:\"expected_compile_log\"`" + String.raw`
	DebugLogs bool                          ` + "`json:\"debug_logs\"`" + String.raw`
	AbsWorkingDir string                    ` + "`json:\"abs_working_dir\"`" + String.raw`
	Options map[string]json.RawMessage      ` + "`json:\"options\"`" + String.raw`
	UnsupportedOptions []string             ` + "`json:\"unsupported_options,omitempty\"`" + String.raw`
}

var bundlerCorpus []bundlerCorpusCase

func captureBundledCase(t *testing.T, s *suite, args bundled, fileSystem string) {
	t.Helper()
	line := 0
	for skip := 1; ; skip++ {
		_, file, candidateLine, ok := runtime.Caller(skip)
		if !ok { break }
		if strings.HasSuffix(file, "_test.go") && !strings.HasSuffix(file, "bundler_test.go") {
			line = candidateLine
			break
		}
	}

	options := make(map[string]json.RawMessage)
	unsupported := []string{}
	value := reflect.ValueOf(args.options)
	typeOfValue := value.Type()
	for i := 0; i < value.NumField(); i++ {
		field := value.Field(i)
		if field.IsZero() { continue }
		data, err := json.Marshal(field.Interface())
		if err != nil {
			unsupported = append(unsupported, typeOfValue.Field(i).Name)
			continue
		}
		options[typeOfValue.Field(i).Name] = data
	}
	sort.Strings(unsupported)
	bundlerCorpus = append(bundlerCorpus, bundlerCorpusCase{
		UpstreamTest: t.Name(),
		Line: line,
		Suite: s.name,
		FileSystem: fileSystem,
		Files: args.files,
		EntryPaths: args.entryPaths,
		EntryPathsAdvanced: args.entryPathsAdvanced,
		ExpectedScanLog: args.expectedScanLog,
		ExpectedCompileLog: args.expectedCompileLog,
		DebugLogs: args.debugLogs,
		AbsWorkingDir: args.absWorkingDir,
		Options: options,
		UnsupportedOptions: unsupported,
	})
}
`;

  fs.writeFileSync(testPath, source);
  childProcess.execFileSync("gofmt", ["-w", testPath]);
  childProcess.execFileSync(
    "go",
    ["test", `./${path.relative(upstreamRoot, tempRoot)}`],
    {
      cwd: upstreamRoot,
      env: { ...process.env, BUNDLER_CORPUS_OUT: corpusPath },
      stdio: "inherit",
    },
  );

  const cases = JSON.parse(fs.readFileSync(corpusPath, "utf8"));
  const snapshots = loadSnapshots(packageRoot);
  for (const testCase of cases) {
    testCase.expected_snapshot = snapshots.get(testCase.suite)?.get(
      testCase.upstream_test,
    );
  }
  fs.writeFileSync(outputPath, `${JSON.stringify(cases, null, 2)}\n`);
  console.log(`generated ${cases.length} upstream bundler cases`);
} finally {
  fs.rmSync(tempRoot, { recursive: true, force: true });
}

function replaceFunctionBody(source, signature, replacement) {
  const signatureOffset = source.indexOf(signature);
  if (signatureOffset < 0) throw new Error(`could not find ${signature.trim()}`);
  const openBrace = source.indexOf("{", signatureOffset + signature.length);
  const closeBrace = matchingDelimiter(source, openBrace, "{", "}");
  return source.slice(0, openBrace) + replacement + source.slice(closeBrace + 1);
}

function matchingDelimiter(text, openIndex, open, close) {
  let depth = 0;
  let state = "code";
  for (let index = openIndex; index < text.length; index += 1) {
    const character = text[index];
    const next = text[index + 1];
    if (state === "double") {
      if (character === "\\") index += 1;
      else if (character === '"') state = "code";
      continue;
    }
    if (state === "raw") {
      if (character === "`") state = "code";
      continue;
    }
    if (state === "line-comment") {
      if (character === "\n") state = "code";
      continue;
    }
    if (state === "block-comment") {
      if (character === "*" && next === "/") {
        state = "code";
        index += 1;
      }
      continue;
    }
    if (character === '"') state = "double";
    else if (character === "`") state = "raw";
    else if (character === "/" && next === "/") {
      state = "line-comment";
      index += 1;
    } else if (character === "/" && next === "*") {
      state = "block-comment";
      index += 1;
    } else if (character === open) depth += 1;
    else if (character === close && --depth === 0) return index;
  }
  throw new Error(`unclosed ${open}`);
}

function loadSnapshots(packageRoot) {
  const splitter =
    "\n================================================================================\n";
  const result = new Map();
  const snapshotsRoot = path.join(packageRoot, "snapshots");
  for (const name of fs.readdirSync(snapshotsRoot)) {
    const match = /^snapshots_(.+)\.txt$/.exec(name);
    if (!match) continue;
    const suite = new Map();
    const contents = fs
      .readFileSync(path.join(snapshotsRoot, name), "utf8")
      .replaceAll("\r\n", "\n");
    for (const part of contents.split(splitter)) {
      const newline = part.indexOf("\n");
      if (newline === -1) suite.set(part, "");
      else suite.set(part.slice(0, newline), part.slice(newline + 1));
    }
    result.set(match[1], suite);
  }
  return result;
}
