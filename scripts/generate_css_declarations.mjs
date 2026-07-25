#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [inputPath, outputPath] = process.argv.slice(2);
if (!inputPath || !outputPath) {
  throw new Error(
    "usage: generate_css_declarations.mjs <upstream-css-decl-table.go> <output.rs>",
  );
}

const source = fs.readFileSync(inputPath, "utf8");
const enumStart = source.indexOf("const (");
const enumEnd = source.indexOf("var KnownDeclarations", enumStart);
const mapEnd = source.indexOf("var typoDetector", enumEnd);
if (enumStart < 0 || enumEnd < 0 || mapEnd < 0) {
  throw new Error("could not locate the upstream CSS declaration table");
}

const declarations = [];
for (const line of source.slice(enumStart, enumEnd).split(/\r?\n/)) {
  const match = line.match(/^\t(D[A-Za-z0-9_]+)/);
  if (match) declarations.push(match[1]);
}

const known = [];
for (const line of source.slice(enumEnd, mapEnd).split(/\r?\n/)) {
  const match = line.match(/^\t"([^"]+)":\s+(D[A-Za-z0-9_]+),$/);
  if (match) known.push({ text: match[1], declaration: match[2] });
}

if (declarations.length !== 329) {
  throw new Error(`expected 329 declaration kinds, found ${declarations.length}`);
}
if (known.length !== 328) {
  throw new Error(`expected 328 known declaration spellings, found ${known.length}`);
}

const rustName = (goName) => goName.slice(1);
const lines = [
  "// Generated from upstream internal/css_ast/css_decl_table.go.",
  "",
  "pub(super) const KNOWN_DECLARATION_PAIRS: &[(&str, Declaration)] = &[",
];
for (const entry of known) {
  lines.push(
    `    (${JSON.stringify(entry.text)}, Declaration::${rustName(entry.declaration)}),`,
  );
}
lines.push("];", "");

const enumLines = [
  "// Generated from upstream internal/css_ast/css_decl_table.go.",
  "",
  "#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]",
  "#[repr(u16)]",
  "pub enum Declaration {",
];
for (const [index, declaration] of declarations.entries()) {
  enumLines.push(
    `    ${index === 0 ? "#[default]\n    " : ""}${rustName(declaration)},`,
  );
}
enumLines.push("}", "", ...lines.slice(2));

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.writeFileSync(outputPath, enumLines.join("\n"));
