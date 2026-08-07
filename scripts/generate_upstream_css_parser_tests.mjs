#!/usr/bin/env node

// Execute an instrumented temporary copy of the pinned upstream css_parser
// tests. This captures generated test cases too (loops, fmt.Sprintf, and local
// variables), which a source-only extractor would otherwise silently miss.

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import childProcess from "node:child_process";

if (process.argv.length !== 4) {
  console.error("usage: node scripts/generate_upstream_css_parser_tests.mjs <upstream-root> <output-json>");
  process.exit(2);
}

const upstreamRoot = path.resolve(process.argv[2]);
const outputPath = path.resolve(process.argv[3]);
const packageRoot = path.join(upstreamRoot, "internal", "css_parser");
const tempRoot = fs.mkdtempSync(path.join(upstreamRoot, "internal", "css_parser_corpus_"));
const tempPackage = tempRoot;
const corpusPath = path.join(tempRoot, "corpus.json");

try {
  for (const name of fs.readdirSync(packageRoot)) {
    if (name.endsWith(".go") && name !== "css_parser_test.go") {
      fs.copyFileSync(path.join(packageRoot, name), path.join(tempPackage, name));
    }
  }

  const testPath = path.join(packageRoot, "css_parser_test.go");
  let source = fs.readFileSync(testPath, "utf8");
  source = source.replace(
    "import (\n",
    "import (\n\t\"encoding/json\"\n\t\"os\"\n\t\"runtime\"\n",
  );
  for (const unusedImport of [
    "\t\"github.com/evanw/esbuild/internal/ast\"\n",
    "\t\"github.com/evanw/esbuild/internal/css_printer\"\n",
    "\t\"github.com/evanw/esbuild/internal/logger\"\n",
    "\t\"github.com/evanw/esbuild/internal/test\"\n",
  ]) {
    source = source.replace(unusedImport, "");
  }

  const signature = "func expectPrintedCommon(t *testing.T, name string, contents string, expected string, expectedLog string, loader config.Loader, options config.Options) ";
  const signatureOffset = source.indexOf(signature);
  if (signatureOffset < 0) throw new Error("could not find expectPrintedCommon");
  const openBrace = source.indexOf("{", signatureOffset + signature.length);
  const closeBrace = matchingDelimiter(source, openBrace, "{", "}");
  const replacement = `{
\tt.Helper()
\tupstreamTest := "unknown"
\tline := 0
\tfor skip := 1; ; skip++ {
\t\tpc, file, candidateLine, ok := runtime.Caller(skip)
\t\tif !ok { break }
\t\tfunction := runtime.FuncForPC(pc)
\t\tif function != nil && strings.HasPrefix(function.Name()[strings.LastIndex(function.Name(), ".")+1:], "Test") {
\t\t\tupstreamTest = function.Name()[strings.LastIndex(function.Name(), ".")+1:]
\t\t\tif strings.HasSuffix(file, "css_parser_test.go") { line = candidateLine }
\t\t\tbreak
\t\t}
\t}
\tcssParserCorpus = append(cssParserCorpus, cssParserCorpusCase{
\t\tUpstreamTest: upstreamTest,
\t\tLine: line,
\t\tName: name,
\t\tSource: []byte(contents),
\t\tExpected: []byte(expected),
\t\tExpectedLog: []byte(expectedLog),
\t\tLoader: uint8(loader),
\t\tMinifySyntax: options.MinifySyntax,
\t\tMinifyWhitespace: options.MinifyWhitespace,
\t\tUnsupportedCSSFeatures: uint16(options.UnsupportedCSSFeatures),
\t\tAllPrefixes: len(options.CSSPrefixData) != 0,
\t})
}`;
  source = source.slice(0, openBrace) + replacement + source.slice(closeBrace + 1);

  source += `

type cssParserCorpusCase struct {
\tUpstreamTest string \`json:"upstream_test"\`
\tLine int \`json:"line"\`
\tName string \`json:"name"\`
\tSource []byte \`json:"source_base64"\`
\tExpected []byte \`json:"expected_base64"\`
\tExpectedLog []byte \`json:"expected_log_base64"\`
\tLoader uint8 \`json:"loader"\`
\tMinifySyntax bool \`json:"minify_syntax"\`
\tMinifyWhitespace bool \`json:"minify_whitespace"\`
\tUnsupportedCSSFeatures uint16 \`json:"unsupported_css_features"\`
\tAllPrefixes bool \`json:"all_prefixes"\`
}

var cssParserCorpus []cssParserCorpusCase

func TestMain(m *testing.M) {
\tcode := m.Run()
\tdata, err := json.MarshalIndent(cssParserCorpus, "", "  ")
\tif err == nil { err = os.WriteFile(os.Getenv("CSS_PARSER_CORPUS_OUT"), append(data, '\\n'), 0644) }
\tif err != nil { panic(err) }
\tos.Exit(code)
}
`;
  fs.writeFileSync(path.join(tempPackage, "css_parser_test.go"), source);

  childProcess.execFileSync(
    "go",
    ["test", `./${path.relative(upstreamRoot, tempPackage)}`],
    {
      cwd: upstreamRoot,
      env: { ...process.env, CSS_PARSER_CORPUS_OUT: corpusPath },
      stdio: "inherit",
    },
  );
  const cases = JSON.parse(fs.readFileSync(corpusPath, "utf8"));
  fs.writeFileSync(outputPath, `${JSON.stringify(cases, null, 2)}\n`);
  console.log(`generated ${cases.length} upstream css_parser cases`);
} finally {
  fs.rmSync(tempRoot, { recursive: true, force: true });
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
      if (character === "*" && next === "/") { state = "code"; index += 1; }
      continue;
    }
    if (character === '"') state = "double";
    else if (character === "`") state = "raw";
    else if (character === "/" && next === "/") { state = "line-comment"; index += 1; }
    else if (character === "/" && next === "*") { state = "block-comment"; index += 1; }
    else if (character === open) depth += 1;
    else if (character === close && --depth === 0) return index;
  }
  throw new Error(`unclosed ${open}`);
}
