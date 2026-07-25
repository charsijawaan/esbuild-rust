#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [sourcePath, outputPath] = process.argv.slice(2);
if (!sourcePath || !outputPath) {
  throw new Error("usage: generate_config_globals.mjs <globals.go> <known_globals.rs>");
}

const source = fs.readFileSync(sourcePath, "utf8");
const startMarker = "var knownGlobals = [][]string{";
const start = source.indexOf(startMarker);
const end = source.indexOf("\n}\n\n// We currently only support", start);
if (start < 0 || end < 0) {
  throw new Error("could not locate knownGlobals");
}

const body = source.slice(start + startMarker.length, end);
const entries = [];
for (const match of body.matchAll(/^\s*\{((?:"(?:[^"\\]|\\.)*"\s*,?\s*)+)\},/gm)) {
  const parts = [...match[1].matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((part) =>
    JSON.parse(`"${part[1]}"`),
  );
  entries.push(parts);
}
if (entries.length < 100) {
  throw new Error(`knownGlobals extraction was incomplete: ${entries.length}`);
}

const lines = [
  "//! Generated from the pinned upstream `internal/config/globals.go`.",
  "",
  "pub const KNOWN_GLOBALS: &[&[&str]] = &[",
];
for (const parts of entries) {
  lines.push(`    &[${parts.map((part) => JSON.stringify(part)).join(", ")}],`);
}
lines.push("];", "");

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.writeFileSync(outputPath, lines.join("\n"));
