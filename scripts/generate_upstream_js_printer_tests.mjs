#!/usr/bin/env node

// Extract table-driven cases from the pinned upstream js_printer test file.
// This script deliberately rejects expressions it cannot evaluate so an
// upstream case can never disappear from the Rust corpus silently.

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const modes = new Map([
  ["expectPrinted", { mode: "default", target: false }],
  ["expectPrintedMinify", { mode: "minify_whitespace", target: false }],
  ["expectPrintedMangle", { mode: "minify_syntax", target: false }],
  ["expectPrintedMangleMinify", { mode: "minify_syntax, minify_whitespace", target: false }],
  ["expectPrintedASCII", { mode: "ascii", target: false }],
  ["expectPrintedMinifyASCII", { mode: "minify_whitespace, ascii", target: false }],
  ["expectPrintedTarget", { mode: "target", target: true }],
  ["expectPrintedTargetMinify", { mode: "target, minify_whitespace", target: true }],
  ["expectPrintedTargetMangle", { mode: "target, minify_syntax", target: true }],
  ["expectPrintedTargetASCII", { mode: "target, ascii", target: true }],
  ["expectPrintedJSX", { mode: "jsx", target: false }],
  ["expectPrintedJSXASCII", { mode: "jsx, ascii", target: false }],
  ["expectPrintedJSXMinify", { mode: "jsx, minify_whitespace", target: false }],
]);

if (process.argv.length !== 4) {
  console.error(
    "usage: node scripts/generate_upstream_js_printer_tests.mjs <upstream-root> <output-json>",
  );
  process.exit(2);
}

const input = path.join(process.argv[2], "internal", "js_printer", "js_printer_test.go");
const output = process.argv[3];
const source = fs.readFileSync(input, "utf8");
const cases = [];
const unsupported = [];
const functionPattern = /^func (Test[A-Za-z0-9_]+)\(t \*testing\.T\) \{/gm;

for (const functionMatch of source.matchAll(functionPattern)) {
  const upstreamTest = functionMatch[1];
  const openBrace = functionMatch.index + functionMatch[0].lastIndexOf("{");
  const closeBrace = matchingDelimiter(source, openBrace, "{", "}");
  const body = source.slice(openBrace + 1, closeBrace);
  const bindings = extractStringBindings(body);
  const callPattern = /\b(expectPrinted(?:MangleMinify|MinifyASCII|TargetMinify|TargetMangle|TargetASCII|JSXMinify|JSXASCII|Minify|Mangle|ASCII|Target|JSX)?)\s*\(/g;

  for (const callMatch of body.matchAll(callPattern)) {
    const helper = callMatch[1];
    const configuration = modes.get(helper);
    if (!configuration) continue;

    const absoluteOpenParen = openBrace + 1 + callMatch.index + callMatch[0].lastIndexOf("(");
    const absoluteCloseParen = matchingDelimiter(source, absoluteOpenParen, "(", ")");
    const argumentsText = source.slice(absoluteOpenParen + 1, absoluteCloseParen);
    const args = splitTopLevel(argumentsText, ",");
    const line = lineNumberAt(source, absoluteOpenParen);
    const sourceIndex = configuration.target ? 2 : 1;
    const expectedIndex = configuration.target ? 3 : 2;

    try {
      if (args.length <= expectedIndex) throw new Error("unexpected argument count");
      const testCase = {
        upstream_test: upstreamTest,
        line,
        mode: configuration.mode,
        source: evaluateString(args[sourceIndex], bindings),
        expected: evaluateString(args[expectedIndex], bindings),
      };
      if (configuration.target) testCase.target = evaluateInteger(args[1]);
      cases.push(testCase);
    } catch (error) {
      unsupported.push(`${input}:${line}: ${error.message}`);
    }
  }
}

if (unsupported.length > 0) {
  for (const message of unsupported) console.error(message);
  console.error(`refusing to generate: ${unsupported.length} upstream calls were not understood`);
  process.exit(1);
}

fs.writeFileSync(output, `${JSON.stringify(cases, null, 2)}\n`);
console.log(`generated ${cases.length} upstream js_printer cases`);

function extractStringBindings(body) {
  const bindings = new Map();
  const bindingPattern = /^\s*([A-Za-z_][A-Za-z0-9_]*)\s*:=\s*(.+)$/gm;
  for (const match of body.matchAll(bindingPattern)) {
    try {
      bindings.set(match[1], evaluateString(match[2], bindings));
    } catch {
      // Non-string local bindings are irrelevant to this corpus extractor.
    }
  }
  return bindings;
}

function evaluateString(expression, bindings = new Map()) {
  const text = stripOuterParentheses(expression.trim());
  const additions = splitTopLevel(text, "+");
  if (additions.length > 1) return additions.map((item) => evaluateString(item, bindings)).join("");

  if (text.startsWith("strings.Repeat(")) {
    const open = text.indexOf("(");
    const close = matchingDelimiter(text, open, "(", ")");
    if (text.slice(close + 1).trim() !== "") throw new Error(`unsupported string expression: ${text}`);
    const args = splitTopLevel(text.slice(open + 1, close), ",");
    if (args.length !== 2) throw new Error(`unsupported strings.Repeat call: ${text}`);
    return evaluateString(args[0], bindings).repeat(evaluateInteger(args[1]));
  }

  if (text.startsWith('"') && text.endsWith('"')) return unquoteGoString(text);
  if (text.startsWith("`") && text.endsWith("`")) return text.slice(1, -1).replaceAll("\r", "");
  if (bindings.has(text)) return bindings.get(text);
  throw new Error(`non-constant string expression: ${text.slice(0, 100)}`);
}

function evaluateInteger(expression) {
  const text = stripOuterParentheses(expression.trim()).replaceAll("_", "");
  if (!/^[0-9]+$/.test(text)) throw new Error(`non-constant integer expression: ${text}`);
  return Number.parseInt(text, 10);
}

function stripOuterParentheses(text) {
  let result = text;
  while (result.startsWith("(")) {
    const close = matchingDelimiter(result, 0, "(", ")");
    if (close !== result.length - 1) break;
    result = result.slice(1, -1).trim();
  }
  return result;
}

function unquoteGoString(text) {
  let result = "";
  for (let index = 1; index < text.length - 1; index += 1) {
    let character = text[index];
    if (character !== "\\") {
      result += character;
      continue;
    }

    index += 1;
    character = text[index];
    const simple = {
      a: "\u0007",
      b: "\b",
      f: "\f",
      n: "\n",
      r: "\r",
      t: "\t",
      v: "\u000b",
      "\\": "\\",
      '"': '"',
      "'": "'",
    };
    if (Object.hasOwn(simple, character)) {
      result += simple[character];
      continue;
    }

    if (character === "x" || character === "u" || character === "U") {
      const count = character === "x" ? 2 : character === "u" ? 4 : 8;
      const digits = text.slice(index + 1, index + 1 + count);
      if (!new RegExp(`^[0-9a-fA-F]{${count}}$`).test(digits)) {
        throw new Error(`invalid Go escape in ${text}`);
      }
      result += String.fromCodePoint(Number.parseInt(digits, 16));
      index += count;
      continue;
    }

    if (/[0-7]/.test(character)) {
      const digits = text.slice(index, index + 3);
      if (!/^[0-7]{3}$/.test(digits)) throw new Error(`invalid Go octal escape in ${text}`);
      result += String.fromCodePoint(Number.parseInt(digits, 8));
      index += 2;
      continue;
    }
    throw new Error(`unknown Go escape \\${character}`);
  }
  return result;
}

function splitTopLevel(text, delimiter) {
  const parts = [];
  let start = 0;
  const stack = [];
  let state = "code";

  for (let index = 0; index < text.length; index += 1) {
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
    } else if ('([{'.includes(character)) stack.push(character);
    else if (')]}'.includes(character)) stack.pop();
    else if (character === delimiter && stack.length === 0) {
      parts.push(text.slice(start, index).trim());
      start = index + 1;
    }
  }
  parts.push(text.slice(start).trim());
  return parts;
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
    else if (character === close) {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  throw new Error(`unclosed ${open} at offset ${openIndex}`);
}

function lineNumberAt(text, index) {
  let line = 1;
  for (let offset = 0; offset < index; offset += 1) {
    if (text.charCodeAt(offset) === 10) line += 1;
  }
  return line;
}
