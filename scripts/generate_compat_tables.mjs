#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [inputPath, outputPath] = process.argv.slice(2);
if (!inputPath || !outputPath) {
  throw new Error("usage: generate_compat_tables.mjs <upstream-js-table.go> <output.rs>");
}

const source = fs.readFileSync(inputPath, "utf8");
const tableStart = source.indexOf("var jsTable = ");
const tableEnd = source.indexOf("// Return all features", tableStart);
if (tableStart < 0 || tableEnd < 0) {
  throw new Error("could not locate the upstream JavaScript compatibility table");
}

function rustFeature(goName) {
  return goName
    .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
    .replace(/([A-Z])([A-Z][a-z])/g, "$1_$2")
    .toUpperCase();
}

function rustEngine(goName) {
  return { ES: "Es", IE: "Ie", IOS: "Ios" }[goName] ?? goName;
}

const features = [];
let current = null;
for (const line of source.slice(tableStart, tableEnd).split(/\r?\n/)) {
  const emptyFeature = line.match(/^\t([A-Za-z0-9_]+): \{\},$/);
  if (emptyFeature) {
    features.push({ name: emptyFeature[1], engines: [] });
    current = null;
    continue;
  }

  const feature = line.match(/^\t([A-Za-z0-9_]+): \{$/);
  if (feature) {
    current = { name: feature[1], engines: [] };
    features.push(current);
    continue;
  }

  const engine = line.match(/^\t\t([A-Za-z0-9_]+):\s+\{(.+)\},$/);
  if (!engine || !current) continue;

  const ranges = [];
  const rangePattern =
    /\{start: v\{(\d+),\s*(\d+),\s*(\d+)\}(?:,\s*end: v\{(\d+),\s*(\d+),\s*(\d+)\})?\}/g;
  for (const match of engine[2].matchAll(rangePattern)) {
    ranges.push(
      match[4] === undefined
        ? `VersionRange::from_start(${match[1]}, ${match[2]}, ${match[3]})`
        : `VersionRange::bounded(${match[1]}, ${match[2]}, ${match[3]}, ${match[4]}, ${match[5]}, ${match[6]})`,
    );
  }
  if (ranges.length === 0) {
    throw new Error(`could not parse version ranges: ${line}`);
  }
  current.engines.push({ name: engine[1], ranges });
}

if (features.length !== 61) {
  throw new Error(`expected 61 JavaScript features, found ${features.length}`);
}

const lines = [
  "// Generated from upstream internal/compat/js_table.go.",
  "",
  "use super::{Engine, FeatureTable, JsFeature, VersionRange};",
  "",
  "pub(super) const JS_TABLE: FeatureTable = &[",
];
for (const feature of features) {
  lines.push(`    (JsFeature::${rustFeature(feature.name)}, &[`);
  for (const engine of feature.engines) {
    lines.push(
      `        (Engine::${rustEngine(engine.name)}, &[${engine.ranges.join(", ")}]),`,
    );
  }
  lines.push("    ]),");
}
lines.push("];", "");

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.writeFileSync(outputPath, lines.join("\n"));
