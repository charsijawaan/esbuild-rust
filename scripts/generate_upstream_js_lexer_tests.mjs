#!/usr/bin/env node

// Extract every table-driven case from the pinned upstream js_lexer tests.
// Inputs are base64-encoded because Go strings (and this Rust port) support
// arbitrary WTF-8 bytes that JSON strings cannot represent losslessly.

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

if (process.argv.length !== 4) {
  console.error("usage: node scripts/generate_upstream_js_lexer_tests.mjs <upstream-root> <output-json>");
  process.exit(2);
}

const inputPath = path.join(process.argv[2], "internal", "js_lexer", "js_lexer_test.go");
const source = fs.readFileSync(inputPath, "utf8");
const cases = [];
const helpers = new Map([
  ["expectLexerError", "error"],
  ["expectHashbang", "hashbang"],
  ["expectIdentifier", "identifier"],
  ["expectNumber", "number"],
  ["expectBigInteger", "bigint"],
  ["expectString", "string"],
  ["expectLexerErrorString", "string_error"],
]);

for (const [helper, kind] of helpers) {
  const pattern = new RegExp(`\\b${helper}\\s*\\(t,`, "g");
  for (const match of source.matchAll(pattern)) {
    const open = source.indexOf("(", match.index);
    const close = matchingDelimiter(source, open, "(", ")");
    const args = splitTopLevel(source.slice(open + 1, close), ",");
    if (args.length !== 3 || args[0].trim() !== "t") {
      throw new Error(`${inputPath}:${lineNumberAt(source, open)}: unsupported ${helper} call`);
    }
    const input = evaluateGoStringBytes(args[1]);
    const testCase = {
      kind,
      line: lineNumberAt(source, open),
      input_base64: input.toString("base64"),
    };
    if (kind === "number") {
      testCase.expected_number = String(evaluateNumber(args[2]));
    } else if (kind === "string") {
      testCase.expected_utf16 = decodeWtf8(evaluateGoStringBytes(args[2]));
    } else {
      testCase.expected_base64 = evaluateGoStringBytes(args[2]).toString("base64");
    }
    cases.push(testCase);
  }
}

const tokenBodyMatch = source.match(/func TestTokens\(t \*testing\.T\) \{([\s\S]*?)\n\}/);
if (!tokenBodyMatch) throw new Error("unable to find TestTokens");
const tokenBodyOffset = tokenBodyMatch.index + tokenBodyMatch[0].indexOf(tokenBodyMatch[1]);
const tokenPattern = /\{\s*((?:"(?:\\.|[^"\\])*"|`[^`]*`))\s*,\s*(T[A-Za-z0-9_]+)\s*\}/g;
for (const match of tokenBodyMatch[1].matchAll(tokenPattern)) {
  cases.push({
    kind: "token",
    line: lineNumberAt(source, tokenBodyOffset + match.index),
    input_base64: evaluateGoStringBytes(match[1]).toString("base64"),
    expected_token: match[2].slice(1),
  });
}

cases.sort((a, b) => a.line - b.line);
fs.writeFileSync(process.argv[3], `${JSON.stringify(cases, null, 2)}\n`);
console.log(`generated ${cases.length} upstream js_lexer cases`);

function evaluateNumber(expression) {
  let text = stripOuterParentheses(expression.trim()).replaceAll("_", "");
  while (/^float64\([\s\S]*\)$/.test(text)) {
    text = stripOuterParentheses(text.slice(text.indexOf("(") + 1, -1).trim());
  }
  if (text === "math.Inf(1)") return Infinity;
  const value = Number(text);
  if (Number.isNaN(value)) throw new Error(`unsupported number expression: ${expression}`);
  return value;
}

function evaluateGoStringBytes(expression) {
  const text = stripOuterParentheses(expression.trim());
  const additions = splitTopLevel(text, "+");
  if (additions.length > 1) {
    return Buffer.concat(additions.map(evaluateGoStringBytes));
  }
  if (text.startsWith("strings.Repeat(")) {
    const open = text.indexOf("(");
    const close = matchingDelimiter(text, open, "(", ")");
    const args = splitTopLevel(text.slice(open + 1, close), ",");
    const count = Number(args[1].replaceAll("_", "").trim());
    const chunk = evaluateGoStringBytes(args[0]);
    return Buffer.concat(Array.from({ length: count }, () => chunk));
  }
  if (text.startsWith("`") && text.endsWith("`")) {
    return Buffer.from(text.slice(1, -1).replaceAll("\r", ""), "utf8");
  }
  if (!text.startsWith('"') || !text.endsWith('"')) {
    throw new Error(`unsupported Go string expression: ${text}`);
  }
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
      continue;
    }
    if (character === "x") {
      const digits = text.slice(index + 1, index + 3);
      if (!/^[0-9a-fA-F]{2}$/.test(digits)) throw new Error(`invalid Go hex escape: ${text}`);
      bytes.push(Number.parseInt(digits, 16));
      index += 2;
      continue;
    }
    if (character === "u" || character === "U") {
      const count = character === "u" ? 4 : 8;
      const digits = text.slice(index + 1, index + 1 + count);
      if (!new RegExp(`^[0-9a-fA-F]{${count}}$`).test(digits)) throw new Error(`invalid Go Unicode escape: ${text}`);
      bytes.push(...Buffer.from(String.fromCodePoint(Number.parseInt(digits, 16)), "utf8"));
      index += count;
      continue;
    }
    if (/[0-7]/.test(character)) {
      const digits = text.slice(index, index + 3);
      if (!/^[0-7]{3}$/.test(digits)) throw new Error(`invalid Go octal escape: ${text}`);
      bytes.push(Number.parseInt(digits, 8));
      index += 2;
      continue;
    }
    throw new Error(`unknown Go escape \\${character}`);
  }
  return Buffer.from(bytes);
}

function decodeWtf8(bytes) {
  const result = [];
  for (let index = 0; index < bytes.length;) {
    const first = bytes[index++];
    let codePoint = first;
    if (first >= 0xC0 && first < 0xE0) {
      codePoint = ((first & 31) << 6) | (bytes[index++] & 63);
    } else if (first >= 0xE0 && first < 0xF0) {
      codePoint = ((first & 15) << 12) | ((bytes[index++] & 63) << 6) | (bytes[index++] & 63);
    } else if (first >= 0xF0) {
      codePoint = ((first & 7) << 18) | ((bytes[index++] & 63) << 12) |
        ((bytes[index++] & 63) << 6) | (bytes[index++] & 63);
    }
    if (codePoint <= 0xFFFF) result.push(codePoint);
    else {
      codePoint -= 0x10000;
      result.push(0xD800 | (codePoint >> 10), 0xDC00 | (codePoint & 1023));
    }
  }
  return result;
}

function stripOuterParentheses(text) {
  let result = text;
  while (result.startsWith("(") && matchingDelimiter(result, 0, "(", ")") === result.length - 1) {
    result = result.slice(1, -1).trim();
  }
  return result;
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
    if (state === "double") {
      if (character === "\\") index += 1;
      else if (character === '"') state = "code";
      continue;
    }
    if (state === "raw") {
      if (character === "`") state = "code";
      continue;
    }
    if (character === '"') state = "double";
    else if (character === "`") state = "raw";
    else if (character === open) depth += 1;
    else if (character === close && --depth === 0) return index;
  }
  throw new Error(`unclosed ${open}`);
}

function lineNumberAt(text, offset) {
  return text.slice(0, offset).split("\n").length;
}
