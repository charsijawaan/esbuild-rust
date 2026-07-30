#!/usr/bin/env node

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, isAbsolute, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const repositoryRoot = resolve(import.meta.dirname, "..");
const executable = (value) =>
  isAbsolute(value) ? value : resolve(repositoryRoot, value);
const rustEsbuild = executable(
  process.env.RUST_ESBUILD ?? "target/release/esbuild",
);
const upstreamEsbuild = process.env.UPSTREAM_ESBUILD
  ? executable(process.env.UPSTREAM_ESBUILD)
  : undefined;

if (!existsSync(rustEsbuild)) {
  throw new Error(
    `Rust executable not found at ${rustEsbuild}; run "cargo build --release --locked" first`,
  );
}
if (upstreamEsbuild && !existsSync(upstreamEsbuild)) {
  throw new Error(`Upstream executable not found at ${upstreamEsbuild}`);
}

const cases = [
  {
    name: "ESM graph + tree shaking",
    files: {
      "entry.js":
        "import { used } from './math.js'; console.log(used(20, 22));\n",
      "math.js":
        "export const used = (a, b) => a + b; export const unused = () => 99;\n",
    },
    args: (output) => [
      "entry.js",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      "--tree-shaking=true",
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: "42",
  },
  {
    name: "CommonJS require",
    files: {
      "entry.cjs":
        "const value = require('./dependency.cjs'); console.log(value.answer);\n",
      "dependency.cjs": "module.exports = { answer: 42 };\n",
    },
    args: (output) => [
      "entry.cjs",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: "42",
  },
  {
    name: "TypeScript modules",
    files: {
      "entry.ts":
        "import { double } from './math'; interface Marker { value: number } const marker: Marker = { value: double(21) }; console.log(marker.value);\n",
      "math.ts": "export const double = (value: number): number => value * 2;\n",
    },
    args: (output) => [
      "entry.ts",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: "42",
  },
  {
    name: "JSON loader",
    files: {
      "entry.js":
        "import data from './data.json'; console.log(`${data.name}:${data.value}`);\n",
      "data.json": '{"name":"port","value":42}\n',
    },
    args: (output) => [
      "entry.js",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: "port:42",
  },
  {
    name: "JSX transform",
    files: {
      "entry.jsx":
        "const React = { createElement: (...args) => args }; const element = <main id=\"app\">hello</main>; console.log(JSON.stringify(element));\n",
    },
    args: (output) => [
      "entry.jsx",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      "--jsx-factory=React.createElement",
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: '["main",{"id":"app"},"hello"]',
  },
  {
    name: "Node built-in external",
    files: {
      "entry.js":
        "import { basename } from 'node:path'; console.log(basename('/tmp/result.txt'));\n",
    },
    args: (output) => [
      "entry.js",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: "result.txt",
  },
  {
    name: "CSS extraction",
    files: {
      "entry.js": "import './style.css'; console.log('css-ok');\n",
      "style.css": ".card { color: red; display: grid }\n",
    },
    args: (output) => [
      "entry.js",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      `--outdir=${output}`,
    ],
    run: (output) => join(output, "entry.js"),
    expected: "css-ok",
    verify: (output) => {
      const css = join(output, "entry.css");
      if (!existsSync(css) || !readFileSync(css, "utf8").includes(".card")) {
        throw new Error("extracted entry.css is missing the .card rule");
      }
    },
  },
  {
    name: "Source map + metafile",
    files: {
      "entry.js": "const answer = 40 + 2; console.log(answer);\n",
    },
    args: (output) => [
      "entry.js",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      "--sourcemap=external",
      `--metafile=${join(output, "meta.json")}`,
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: "42",
    verify: (output) => {
      const sourceMap = join(output, "bundle.cjs.map");
      const metafile = join(output, "meta.json");
      if (!existsSync(sourceMap) || !existsSync(metafile)) {
        throw new Error("source map or metafile was not emitted");
      }
      JSON.parse(readFileSync(sourceMap, "utf8"));
      JSON.parse(readFileSync(metafile, "utf8"));
    },
  },
  {
    name: "ESM code splitting",
    files: {
      "entry-a.js":
        "import('./shared.js').then(({ value }) => console.log('a', value));\n",
      "entry-b.js":
        "import('./shared.js').then(({ value }) => console.log('b', value));\n",
      "shared.js": "export const value = 42;\n",
    },
    args: (output) => [
      "entry-a.js",
      "entry-b.js",
      "--bundle",
      "--platform=node",
      "--format=esm",
      "--splitting",
      "--out-extension:.js=.mjs",
      `--outdir=${output}`,
    ],
    run: (output) => join(output, "entry-a.mjs"),
    expected: "a 42",
    verify: (output) => {
      const chunks = readdirSync(output).filter(
        (name) =>
          name.endsWith(".mjs") &&
          name !== "entry-a.mjs" &&
          name !== "entry-b.mjs",
      );
      if (chunks.length === 0) {
        throw new Error("no shared chunk was emitted");
      }
    },
  },
  {
    name: "stdin CommonJS transform",
    files: {},
    args: () => ["--loader=js", "--format=cjs"],
    stdin: "export const answer = 42; console.log(module.exports.answer);\n",
    stdoutFile: (output) => join(output, "transform.cjs"),
    run: (output) => join(output, "transform.cjs"),
    expected: "42",
    verify: (output) => {
      const code = readFileSync(join(output, "transform.cjs"), "utf8");
      if (!code.includes("module.exports = __toCommonJS")) {
        throw new Error("CommonJS transform did not publish ESM exports");
      }
      if (Buffer.byteLength(code) >= 2_000) {
        throw new Error("CommonJS transform retained too much helper runtime");
      }
    },
  },
  {
    name: "stdin IIFE global",
    files: {},
    args: () => [
      "--loader=js",
      "--format=iife",
      "--global-name=Result",
    ],
    stdin:
      "export const answer = 42; Promise.resolve().then(() => console.log(Result.answer));\n",
    stdoutFile: (output) => join(output, "transform.cjs"),
    run: (output) => join(output, "transform.cjs"),
    expected: "42",
    verify: (output) => {
      const code = readFileSync(join(output, "transform.cjs"), "utf8");
      if (!code.includes("var Result = (() => {")) {
        throw new Error("IIFE transform did not assign the requested global");
      }
    },
  },
  {
    name: "transform ignores tsconfig",
    files: {
      "tsconfig.json": '{"compilerOptions":{"jsxFactory":"ambient"}}\n',
    },
    args: () => ["--loader=jsx", "--format=esm"],
    stdin:
      "const React = { createElement: (tag, props) => [tag, props] }; console.log(JSON.stringify(<div />));\n",
    stdoutFile: (output) => join(output, "transform.mjs"),
    run: (output) => join(output, "transform.mjs"),
    expected: '["div",null]',
    verify: (output) => {
      const code = readFileSync(join(output, "transform.mjs"), "utf8");
      if (!code.includes('React.createElement("div", null)')) {
        throw new Error("formatted transform discovered the ambient tsconfig");
      }
    },
  },
  {
    name: "absolute path controls",
    files: {
      "entry.js": "console.log(42);\n",
    },
    args: (output) => [
      "entry.js",
      "--bundle",
      "--platform=node",
      "--format=cjs",
      "--abs-paths=code,metafile",
      `--metafile=${join(output, "meta.json")}`,
      `--outfile=${join(output, "bundle.cjs")}`,
    ],
    run: (output) => join(output, "bundle.cjs"),
    expected: "42",
    verify: (output, caseRoot) => {
      const absoluteEntry = realpathSync(join(caseRoot, "entry.js"));
      const code = readFileSync(join(output, "bundle.cjs"), "utf8");
      const metafile = JSON.parse(
        readFileSync(join(output, "meta.json"), "utf8"),
      );
      if (!code.includes(`// ${absoluteEntry}`)) {
        throw new Error("generated code did not use the absolute source path");
      }
      if (!Object.hasOwn(metafile.inputs, absoluteEntry)) {
        throw new Error("metafile did not use the absolute input path");
      }
    },
  },
];

const byteSize = (path) => {
  if (!existsSync(path)) return 0;
  const metadata = statSync(path);
  if (metadata.isFile()) return metadata.size;
  return readdirSync(path).reduce(
    (total, child) => total + byteSize(join(path, child)),
    0,
  );
};

const runCase = (binary, testCase, caseRoot, output) => {
  mkdirSync(output, { recursive: true });
  const build = spawnSync(binary, testCase.args(output), {
    cwd: caseRoot,
    encoding: "utf8",
    input: testCase.stdin,
    timeout: 30_000,
  });
  if (build.status !== 0) {
    return {
      ok: false,
      detail: `build exited ${build.status}: ${build.stderr.trim()}`,
      stdout: "",
      bytes: byteSize(output),
    };
  }
  if (testCase.stdoutFile) {
    writeFileSync(testCase.stdoutFile(output), build.stdout);
  }

  const runFile = testCase.run(output);
  const runtime = spawnSync(process.execPath, [runFile], {
    cwd: caseRoot,
    encoding: "utf8",
    timeout: 30_000,
  });
  if (runtime.status !== 0) {
    return {
      ok: false,
      detail: `runtime exited ${runtime.status}: ${runtime.stderr.trim()}`,
      stdout: runtime.stdout.trim(),
      bytes: byteSize(output),
    };
  }

  const stdout = runtime.stdout.trim();
  if (stdout !== testCase.expected) {
    return {
      ok: false,
      detail: `expected ${JSON.stringify(testCase.expected)}, received ${JSON.stringify(stdout)}`,
      stdout,
      bytes: byteSize(output),
    };
  }

  try {
    testCase.verify?.(output, caseRoot);
  } catch (error) {
    return {
      ok: false,
      detail: error.message,
      stdout,
      bytes: byteSize(output),
    };
  }

  return { ok: true, detail: "", stdout, bytes: byteSize(output) };
};

const temporaryRoot = mkdtempSync(join(tmpdir(), "esbuild-rs-evaluation-"));
const results = [];

try {
  for (const [index, testCase] of cases.entries()) {
    const caseRoot = join(
      temporaryRoot,
      `${String(index + 1).padStart(2, "0")}-${testCase.name
        .toLowerCase()
        .replaceAll(/[^a-z0-9]+/g, "-")}`,
    );
    mkdirSync(caseRoot, { recursive: true });
    for (const [path, contents] of Object.entries(testCase.files)) {
      const absolutePath = join(caseRoot, path);
      mkdirSync(resolve(absolutePath, ".."), { recursive: true });
      writeFileSync(absolutePath, contents);
    }

    const rust = runCase(
      rustEsbuild,
      testCase,
      caseRoot,
      join(caseRoot, "rust-output"),
    );
    const upstream = upstreamEsbuild
      ? runCase(
          upstreamEsbuild,
          testCase,
          caseRoot,
          join(caseRoot, "upstream-output"),
        )
      : undefined;
    results.push({ name: testCase.name, rust, upstream });
  }
} finally {
  rmSync(temporaryRoot, { recursive: true, force: true });
}

console.log(
  `Rust: ${basename(rustEsbuild)} | Upstream: ${
    upstreamEsbuild ? basename(upstreamEsbuild) : "not configured"
  }`,
);
console.log("");
for (const result of results) {
  const rustStatus = result.rust.ok ? "PASS" : "FAIL";
  const upstreamStatus = result.upstream
    ? result.upstream.ok
      ? "PASS"
      : "FAIL"
    : "SKIP";
  const parity = result.upstream
    ? result.rust.ok &&
      result.upstream.ok &&
      result.rust.stdout === result.upstream.stdout
      ? "yes"
      : "no"
    : "n/a";
  console.log(
    `${result.name.padEnd(27)} rust=${rustStatus} upstream=${upstreamStatus} runtime-parity=${parity}`,
  );
  console.log(
    `  bytes: rust=${result.rust.bytes}${
      result.upstream ? ` upstream=${result.upstream.bytes}` : ""
    }`,
  );
  if (!result.rust.ok) console.log(`  rust detail: ${result.rust.detail}`);
  if (result.upstream && !result.upstream.ok) {
    console.log(`  upstream detail: ${result.upstream.detail}`);
  }
}

const rustPassed = results.filter((result) => result.rust.ok).length;
const parityPassed = results.filter(
  (result) =>
    result.upstream &&
    result.rust.ok &&
    result.upstream.ok &&
    result.rust.stdout === result.upstream.stdout,
).length;
console.log("");
console.log(`Rust scenarios: ${rustPassed}/${results.length} passed`);
if (upstreamEsbuild) {
  console.log(`Runtime parity: ${parityPassed}/${results.length} scenarios`);
}

if (
  rustPassed !== results.length ||
  (upstreamEsbuild && parityPassed !== results.length)
) {
  process.exitCode = 1;
}
