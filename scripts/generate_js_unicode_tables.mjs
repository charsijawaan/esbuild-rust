#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [inputPath, outputPath] = process.argv.slice(2);
if (!inputPath || !outputPath) {
  throw new Error(
    "usage: generate_js_unicode_tables.mjs <upstream-unicode.go> <output.rs>",
  );
}

const source = fs.readFileSync(inputPath, "utf8");
const tables = [
  ["idStartES5AndESNext", "ID_START_ES5_AND_ES_NEXT"],
  ["idContinueES5AndESNext", "ID_CONTINUE_ES5_AND_ES_NEXT"],
  ["idStartES5OrESNext", "ID_START_ES5_OR_ES_NEXT"],
  ["idContinueES5OrESNext", "ID_CONTINUE_ES5_OR_ES_NEXT"],
];

const output = [
  "// Generated from upstream internal/js_ast/unicode.go.",
  "",
  "pub(super) type UnicodeRange = (u32, u32, u32);",
  "",
];

for (let index = 0; index < tables.length; index++) {
  const [goName, rustName] = tables[index];
  const start = source.indexOf(`var ${goName} = `);
  const end =
    index + 1 < tables.length
      ? source.indexOf(`var ${tables[index + 1][0]} = `, start)
      : source.length;
  if (start < 0 || end < 0) {
    throw new Error(`could not locate Unicode table ${goName}`);
  }
  const ranges = [];
  const pattern =
    /\{Lo: (0x[0-9a-f]+), Hi: (0x[0-9a-f]+), Stride: (\d+)\}/g;
  for (const match of source.slice(start, end).matchAll(pattern)) {
    ranges.push([match[1], match[2], match[3]]);
  }
  if (ranges.length === 0) {
    throw new Error(`Unicode table ${goName} was empty`);
  }
  output.push(`pub(super) const ${rustName}: &[UnicodeRange] = &[`);
  for (const [lo, hi, stride] of ranges) {
    output.push(`    (${lo}, ${hi}, ${stride}),`);
  }
  output.push("];", "");
}

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.writeFileSync(outputPath, output.join("\n"));
