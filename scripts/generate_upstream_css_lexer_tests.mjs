#!/usr/bin/env node

// Extract the complete pinned upstream css_lexer test matrix.

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

if (process.argv.length !== 4) {
  console.error("usage: node scripts/generate_upstream_css_lexer_tests.mjs <upstream-root> <output-json>");
  process.exit(2);
}

const inputPath = path.join(process.argv[2], "internal", "css_lexer", "css_lexer_test.go");
const source = fs.readFileSync(inputPath, "utf8");
const cases = [];
const literal = String.raw`("(?:\\.|[^"\\])*")`;

const tokenPattern = new RegExp(`\\{\\s*${literal}\\s*,\\s*${literal}\\s*,\\s*(T[A-Za-z0-9_]+)\\s*\\}`, "g");
for (const match of source.matchAll(tokenPattern)) {
  cases.push({
    kind: "token",
    line: lineNumberAt(source, match.index),
    input_base64: goStringBytes(match[1]).toString("base64"),
    expected_token: rustTokenName(match[3]),
  });
}

const stringPattern = new RegExp(`contentsOfStringToken\\(${literal}\\)\\s*,\\s*${literal}`, "g");
for (const match of source.matchAll(stringPattern)) {
  cases.push({
    kind: "decoded",
    line: lineNumberAt(source, match.index),
    input_base64: goStringBytes(match[1]).toString("base64"),
    expected_token: "String",
    expected_base64: goStringBytes(match[2]).toString("base64"),
  });
}

const urlPattern = new RegExp(`contentsOfURLToken\\((T[A-Za-z0-9_]+)\\s*,\\s*${literal}\\)\\s*,\\s*${literal}`, "g");
for (const match of source.matchAll(urlPattern)) {
  cases.push({
    kind: "decoded",
    line: lineNumberAt(source, match.index),
    input_base64: goStringBytes(match[2]).toString("base64"),
    expected_token: rustTokenName(match[1]),
    expected_base64: goStringBytes(match[3]).toString("base64"),
  });
}

const errorPattern = new RegExp(`lexerError\\(${literal}\\)\\s*,\\s*${literal}`, "g");
for (const match of source.matchAll(errorPattern)) {
  cases.push({
    kind: "diagnostic",
    line: lineNumberAt(source, match.index),
    input_base64: goStringBytes(match[1]).toString("base64"),
    expected_base64: goStringBytes(match[2]).toString("base64"),
  });
}

const bomMatch = source.match(/lexToken\(("\\uFEFF\.")\)/);
if (!bomMatch) throw new Error("unable to find BOM case");
cases.push({
  kind: "token",
  line: lineNumberAt(source, bomMatch.index),
  input_base64: goStringBytes(bomMatch[1]).toString("base64"),
  expected_token: "DelimDot",
});

cases.sort((a, b) => a.line - b.line);
if (cases.length !== 69) throw new Error(`expected 69 cases, extracted ${cases.length}`);
fs.writeFileSync(process.argv[3], `${JSON.stringify(cases, null, 2)}\n`);
console.log(`generated ${cases.length} upstream css_lexer cases`);

function goStringBytes(text) {
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

function lineNumberAt(text, offset) {
  return text.slice(0, offset).split("\n").length;
}

function rustTokenName(name) {
  return ({ TBadURL: "BadUrl", TCDC: "Cdc", TCDO: "Cdo", TURL: "Url" })[name] ?? name.slice(1);
}
