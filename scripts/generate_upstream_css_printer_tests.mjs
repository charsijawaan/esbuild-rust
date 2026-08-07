#!/usr/bin/env node

// Extract every direct case from the pinned upstream css_printer tests.

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

if (process.argv.length !== 4) {
  console.error("usage: node scripts/generate_upstream_css_printer_tests.mjs <upstream-root> <output-json>");
  process.exit(2);
}

const inputPath = path.join(process.argv[2], "internal", "css_printer", "css_printer_test.go");
const source = fs.readFileSync(inputPath, "utf8");
const cases = [];
const modes = new Map([
  ["expectPrinted", "default"],
  ["expectPrintedMinify", "minify_whitespace"],
  ["expectPrintedASCII", "ascii"],
  ["expectPrintedString", "string"],
]);
const functions = /^func (Test[A-Za-z0-9_]+)\(t \*testing\.T\) \{/gm;

for (const functionMatch of source.matchAll(functions)) {
  const upstreamTest = functionMatch[1];
  const openBrace = functionMatch.index + functionMatch[0].lastIndexOf("{");
  const closeBrace = matchingDelimiter(source, openBrace, "{", "}");
  const body = source.slice(openBrace + 1, closeBrace);
  const bodyOffset = openBrace + 1;
  const helpers = /\b(expectPrintedString|expectPrintedMinify|expectPrintedASCII|expectPrinted)\s*\(/g;
  for (const helperMatch of body.matchAll(helpers)) {
    const helper = helperMatch[1];
    const open = bodyOffset + helperMatch.index + helperMatch[0].lastIndexOf("(");
    const close = matchingDelimiter(source, open, "(", ")");
    const args = splitTopLevel(source.slice(open + 1, close), ",");
    const line = lineNumberAt(source, open);
    if (args.length !== 3 || args[0].trim() !== "t") {
      throw new Error(`${inputPath}:${line}: unsupported ${helper} call`);
    }
    cases.push({
      upstream_test: upstreamTest,
      line,
      mode: modes.get(helper),
      source_base64: goStringBytes(args[1]).toString("base64"),
      expected_base64: goStringBytes(args[2]).toString("base64"),
    });
  }
}

cases.sort((a, b) => a.line - b.line);
fs.writeFileSync(process.argv[3], `${JSON.stringify(cases, null, 2)}\n`);
console.log(`generated ${cases.length} upstream css_printer cases`);

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
    else if ("([{".includes(character)) stack.push(character);
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
