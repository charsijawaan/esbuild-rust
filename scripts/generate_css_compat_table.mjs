#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [inputPath, outputPath] = process.argv.slice(2);
if (!inputPath || !outputPath) {
  throw new Error(
    "usage: generate_css_compat_table.mjs <upstream-css-table.go> <output.rs>",
  );
}

const source = fs.readFileSync(inputPath, "utf8");

function rustName(goName) {
  return goName
    .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
    .replace(/([A-Z])([A-Z][a-z])/g, "$1_$2")
    .toUpperCase();
}

function rustEngine(goName) {
  return { ES: "Es", IE: "Ie", IOS: "Ios" }[goName] ?? goName;
}

function parseVersionRanges(text) {
  const ranges = [];
  const pattern =
    /\{start: v\{(\d+),\s*(\d+),\s*(\d+)\}(?:,\s*end: v\{(\d+),\s*(\d+),\s*(\d+)\})?\}/g;
  for (const match of text.matchAll(pattern)) {
    ranges.push(
      match[4] === undefined
        ? `VersionRange::from_start(${match[1]}, ${match[2]}, ${match[3]})`
        : `VersionRange::bounded(${match[1]}, ${match[2]}, ${match[3]}, ${match[4]}, ${match[5]}, ${match[6]})`,
    );
  }
  return ranges;
}

const featureTableStart = source.indexOf("var cssTable = ");
const featureTableEnd = source.indexOf("// Return all features", featureTableStart);
if (featureTableStart < 0 || featureTableEnd < 0) {
  throw new Error("could not locate the upstream CSS compatibility table");
}

const features = [];
let currentFeature = null;
for (const line of source
  .slice(featureTableStart, featureTableEnd)
  .split(/\r?\n/)) {
  const emptyFeature = line.match(/^\t([A-Za-z0-9_]+): \{\},$/);
  if (emptyFeature) {
    features.push({ name: emptyFeature[1], engines: [] });
    currentFeature = null;
    continue;
  }
  const feature = line.match(/^\t([A-Za-z0-9_]+): \{$/);
  if (feature) {
    currentFeature = { name: feature[1], engines: [] };
    features.push(currentFeature);
    continue;
  }
  const engine = line.match(/^\t\t([A-Za-z0-9_]+):\s+\{(.+)\},$/);
  if (!engine || !currentFeature) continue;
  const ranges = parseVersionRanges(engine[2]);
  if (ranges.length === 0) {
    throw new Error(`could not parse CSS version ranges: ${line}`);
  }
  currentFeature.engines.push({ name: engine[1], ranges });
}
if (features.length !== 13) {
  throw new Error(`expected 13 CSS features, found ${features.length}`);
}

const prefixTableStart = source.indexOf("var cssPrefixTable = ");
const prefixTableEnd = source.indexOf("func CSSPrefixData", prefixTableStart);
if (prefixTableStart < 0 || prefixTableEnd < 0) {
  throw new Error("could not locate the upstream CSS prefix table");
}

const properties = [];
let currentProperty = null;
for (const line of source.slice(prefixTableStart, prefixTableEnd).split(/\r?\n/)) {
  const property = line.match(/^\tcss_ast\.D([A-Za-z0-9_]+): \{$/);
  if (property) {
    currentProperty = { name: property[1], items: [] };
    properties.push(currentProperty);
    continue;
  }
  const item = line.match(
    /^\t\t\{engine: ([A-Za-z0-9_]+), prefix: ([A-Za-z0-9_]+)(?:, withoutPrefix: v\{(\d+),\s*(\d+),\s*(\d+)\})?\},$/,
  );
  if (!item || !currentProperty) continue;
  currentProperty.items.push({
    engine: rustEngine(item[1]),
    prefix: rustName(item[2].replace(/Prefix$/, "")),
    version:
      item[3] === undefined
        ? null
        : `Version::new(${item[3]}, ${item[4]}, ${item[5]})`,
  });
}
if (properties.length !== 33) {
  throw new Error(`expected 33 prefixed CSS properties, found ${properties.length}`);
}

const lines = [
  "// Generated from upstream internal/compat/css_table.go.",
  "",
  "use super::{CssFeature, CssFeatureTable, CssPrefix, Engine, PrefixData, Version, VersionRange};",
  "use crate::internal::css_ast::Declaration;",
  "",
  "pub(super) const CSS_TABLE: CssFeatureTable = &[",
];
for (const feature of features) {
  lines.push(`    (CssFeature::${rustName(feature.name)}, &[`);
  for (const engine of feature.engines) {
    lines.push(
      `        (Engine::${rustEngine(engine.name)}, &[${engine.ranges.join(", ")}]),`,
    );
  }
  lines.push("    ]),");
}
lines.push("];", "", "pub(super) const CSS_PREFIX_TABLE: &[(Declaration, &[PrefixData])] = &[");
for (const property of properties) {
  lines.push(`    (Declaration::${property.name}, &[`);
  for (const item of property.items) {
    const version = item.version ?? "Version::new(0, 0, 0)";
    lines.push(
      `        PrefixData { engine: Engine::${item.engine}, without_prefix: ${version}, prefix: CssPrefix::${item.prefix} },`,
    );
  }
  lines.push("    ]),");
}
lines.push("];", "");

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.writeFileSync(outputPath, lines.join("\n"));
