#!/usr/bin/env node

// Extract the complete pinned upstream json_parser_test.go matrix. Raw inputs
// and expected outputs are base64-encoded so control bytes remain lossless.

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

if (process.argv.length !== 4) {
  console.error("usage: node scripts/generate_upstream_json_parser_tests.mjs <upstream-root> <output-json>");
  process.exit(2);
}

const inputPath = path.join(process.argv[2], "internal", "js_parser", "json_parser_test.go");
const source = fs.readFileSync(inputPath, "utf8");
const cases = [];
const functions = /^func (Test[A-Za-z0-9_]+)\(t \*testing\.T\) \{/gm;

for (const functionMatch of source.matchAll(functions)) {
  const openBrace = functionMatch.index + functionMatch[0].lastIndexOf("{");
  const closeBrace = matchingDelimiter(source, openBrace, "{", "}");
  const body = source.slice(openBrace + 1, closeBrace);
  const bodyOffset = openBrace + 1;
  const helpers = /\b(expectPrintedJSONWithWarning|expectPrintedJSON|expectParseErrorJSON)\s*\(/g;
  for (const helperMatch of body.matchAll(helpers)) {
    const helper = helperMatch[1];
    const open = bodyOffset + helperMatch.index + helperMatch[0].lastIndexOf("(");
    const close = matchingDelimiter(source, open, "(", ")");
    const args = splitTopLevel(source.slice(open + 1, close), ",");
    const line = lineNumberAt(source, open);
    if (args[1]?.trim().startsWith("fmt.Sprintf(")) {
      for (let value = 0; value < 0x20; value += 1) {
        if (value === 10 || value === 13) continue;
        cases.push({
          kind: "error",
          line,
          input_base64: Buffer.from([34, value, 34]).toString("base64"),
          warning_base64: "",
          expected_base64: Buffer.from(`<stdin>: ERROR: Syntax error "\\x${value.toString(16).toUpperCase().padStart(2, "0")}"\n`).toString("base64"),
        });
      }
      continue;
    }
    const expectedArg = helper === "expectPrintedJSONWithWarning" ? 3 : 2;
    const warningArg = helper === "expectPrintedJSONWithWarning" ? 2 : null;
    if (args.length <= expectedArg) throw new Error(`${inputPath}:${line}: unsupported ${helper} call`);
    cases.push({
      kind: helper === "expectParseErrorJSON" ? "error" : "printed",
      line,
      input_base64: goStringBytes(args[1]).toString("base64"),
      warning_base64: warningArg === null ? "" : goStringBytes(args[warningArg]).toString("base64"),
      expected_base64: goStringBytes(args[expectedArg]).toString("base64"),
    });
  }
}

cases.sort((a, b) => a.line - b.line);
fs.writeFileSync(process.argv[3], `${JSON.stringify(cases, null, 2)}\n`);
console.log(`generated ${cases.length} upstream json_parser cases`);

function goStringBytes(expression) {
  const text = expression.trim();
  if (!text.startsWith('"') || !text.endsWith('"')) throw new Error(`unsupported Go string: ${text}`);
  const bytes = [];
  for (let index = 1; index < text.length - 1; index += 1) {
    let character = text[index];
    if (character !== "\\") {
      const codePoint = text.codePointAt(index);
      character = String.fromCodePoint(codePoint);
      bytes.push(...Buffer.from(character, "utf8"));
      index += character.length - 1;
      continue;
    }
    character = text[++index];
    const simple = { a: 7, b: 8, f: 12, n: 10, r: 13, t: 9, v: 11, "\\": 92, '"': 34, "'": 39 };
    if (Object.hasOwn(simple, character)) {
      bytes.push(simple[character]);
    } else if (character === "x") {
      bytes.push(Number.parseInt(text.slice(index + 1, index + 3), 16));
      index += 2;
    } else if (character === "u" || character === "U") {
      const count = character === "u" ? 4 : 8;
      bytes.push(...Buffer.from(String.fromCodePoint(Number.parseInt(text.slice(index + 1, index + 1 + count), 16)), "utf8"));
      index += count;
    } else if (/[0-7]/.test(character)) {
      bytes.push(Number.parseInt(text.slice(index, index + 3), 8));
      index += 2;
    } else {
      throw new Error(`unknown Go escape \\${character}`);
    }
  }
  return Buffer.from(bytes);
}

function splitTopLevel(text, delimiter) {
  const parts = [];
  const stack = [];
  let start = 0;
  let state = "code";
  for (let index = 0; index < text.length; index += 1) {
    const character = text[index];
    const next = text[index + 1];
    if (state === "double") {
      if (character === "\\") index += 1;
      else if (character === '"') state = "code";
      continue;
    }
    if (state === "line-comment") {
      if (character === "\n") state = "code";
      continue;
    }
    if (character === '"') state = "double";
    else if (character === "/" && next === "/") { state = "line-comment"; index += 1; }
    else if ("([{\u007b".includes(character)) stack.push(character);
    else if (")]}".includes(character)) stack.pop();
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
    if (state === "line-comment") {
      if (character === "\n") state = "code";
      continue;
    }
    if (state === "block-comment") {
      if (character === "*" && next === "/") { state = "code"; index += 1; }
      continue;
    }
    if (character === '"') state = "double";
    else if (character === "/" && next === "/") { state = "line-comment"; index += 1; }
    else if (character === "/" && next === "*") { state = "block-comment"; index += 1; }
    else if (character === open) depth += 1;
    else if (character === close && --depth === 0) return index;
  }
  throw new Error(`unclosed ${open}`);
}

function lineNumberAt(text, offset) {
  return text.slice(0, offset).split("\n").length;
}
