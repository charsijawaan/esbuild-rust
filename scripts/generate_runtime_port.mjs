#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [sourcePath, outputPath] = process.argv.slice(2);
if (!sourcePath || !outputPath) {
  throw new Error("usage: generate_runtime_port.mjs <runtime.go> <runtime.rs>");
}

const source = fs.readFileSync(sourcePath, "utf8");
const start = source.indexOf("\ttext := `");
const end = source.indexOf("\n\treturn logger.Source{", start);
if (start < 0 || end < 0) {
  throw new Error("could not locate runtime source assembly");
}

const featureNames = new Map([
  ["ForOf", "FOR_OF"],
  ["ConstAndLet", "CONST_AND_LET"],
  ["ObjectExtensions", "OBJECT_EXTENSIONS"],
  ["ObjectAccessors", "OBJECT_ACCESSORS"],
]);

function translateCondition(condition) {
  return condition.replace(
    /unsupportedJSFeatures\.Has\(compat\.([A-Za-z0-9]+)\)/g,
    (_, name) => {
      const rustName = featureNames.get(name);
      if (!rustName) throw new Error(`unknown runtime feature: ${name}`);
      return `unsupported_js_features.contains(JsFeature::${rustName})`;
    },
  );
}

function rawString(text) {
  for (let hashes = 1; ; hashes++) {
    const fence = "#".repeat(hashes);
    if (!text.includes(`"${fence}`)) {
      return `r${fence}"${text}"${fence}`;
    }
  }
}

const body = source.slice(start, end);
const lines = [
  "//! Port of `internal/runtime`.",
  "//!",
  "//! Generated from the pinned upstream `runtime.go`; do not edit by hand.",
  "#![allow(",
  "    clippy::if_not_else,",
  "    clippy::needless_raw_string_hashes,",
  "    clippy::too_many_lines",
  ")]",
  "",
  "use crate::internal::compat::JsFeature;",
  "use crate::internal::logger::{Path, PrettyPaths, Source};",
  "use std::sync::Arc;",
  "",
  "pub const SOURCE_INDEX: u32 = 0;",
  "",
  "#[must_use]",
  "pub fn source(unsupported_js_features: JsFeature) -> Source {",
  "    let mut text = String::new();",
];

let position = 0;
let indent = 1;
let sawInitialText = false;

function emit(line) {
  lines.push(`${"    ".repeat(indent)}${line}`);
}

while (position < body.length) {
  const rest = body.slice(position);
  const whitespace = rest.match(/^(?:\s+|\/\/[^\n]*(?:\n|$))+/);
  if (whitespace) {
    position += whitespace[0].length;
    continue;
  }

  const text = body.slice(position).match(/^(?:text :=|text \+=) `([\s\S]*?)`/);
  if (text) {
    emit(`text.push_str(${rawString(text[1])});`);
    sawInitialText = true;
    position += text[0].length;
    continue;
  }

  const condition = body.slice(position).match(/^if ([^{]+) \{/);
  if (condition) {
    emit(`if ${translateCondition(condition[1].trim())} {`);
    indent++;
    position += condition[0].length;
    continue;
  }

  if (body.startsWith("} else {", position)) {
    indent--;
    emit("} else {");
    indent++;
    position += "} else {".length;
    continue;
  }

  if (body[position] === "}") {
    indent--;
    emit("}");
    position++;
    continue;
  }

  throw new Error(
    `unrecognized runtime assembly near: ${JSON.stringify(body.slice(position, position + 80))}`,
  );
}

if (!sawInitialText || indent !== 1) {
  throw new Error("runtime source assembly was incomplete");
}

lines.push(
  "",
  "    Source {",
  "        index: SOURCE_INDEX,",
  '        key_path: Path { text: "<runtime>".into(), ..Path::default() },',
  '        pretty_paths: PrettyPaths { abs: "<runtime>".into(), rel: "<runtime>".into() },',
  '        identifier_name: "runtime".into(),',
  "        contents: Arc::from(text.into_bytes()),",
  "    }",
  "}",
  "",
  "#[cfg(test)]",
  "mod tests {",
  "    use super::{SOURCE_INDEX, source};",
  "    use crate::internal::compat::JsFeature;",
  "",
  "    #[test]",
  "    fn source_metadata_matches_upstream() {",
  "        let source = source(JsFeature::NONE);",
  "        assert_eq!(source.index, SOURCE_INDEX);",
  '        assert_eq!(source.key_path.text, "<runtime>");',
  '        assert_eq!(source.pretty_paths.abs, "<runtime>");',
  '        assert_eq!(source.pretty_paths.rel, "<runtime>");',
  '        assert_eq!(source.identifier_name, "runtime");',
  "    }",
  "",
  "    #[test]",
  "    fn feature_gates_select_legacy_syntax() {",
  "        let modern = source(JsFeature::NONE);",
  "        let legacy_for_of = source(JsFeature::FOR_OF);",
  "        let modern_text = String::from_utf8_lossy(&modern.contents);",
  "        let legacy_text = String::from_utf8_lossy(&legacy_for_of.contents);",
  '        assert!(modern_text.contains("for (var prop of __getOwnPropSymbols(b))"));',
  '        assert!(legacy_text.contains("for (var props = __getOwnPropSymbols(b), i = 0"));',
  "        assert_ne!(modern.contents, legacy_for_of.contents);",
  "    }",
  "",
  "    #[test]",
  "    fn feature_gates_cover_loop_and_object_syntax() {",
  "        let modern = source(JsFeature::NONE);",
  "        let legacy_let = source(JsFeature::CONST_AND_LET);",
  "        let legacy_object = source(JsFeature::OBJECT_ACCESSORS | JsFeature::OBJECT_EXTENSIONS);",
  "        let modern_text = String::from_utf8_lossy(&modern.contents);",
  "        let legacy_let_text = String::from_utf8_lossy(&legacy_let.contents);",
  "        let legacy_object_text = String::from_utf8_lossy(&legacy_object.contents);",
  '        assert!(modern_text.contains("for (let key of __getOwnPropNames(from))"));',
  '        assert!(legacy_let_text.contains("for (var keys = __getOwnPropNames(from), i = 0"));',
  '        assert!(modern_text.contains("get [name]()"));',
  '        assert!(legacy_object_text.contains("get: () => __privateGet(this, extra)"));',
  '        assert!(modern_text.contains("set _(value)"));',
  '        assert!(legacy_object_text.contains("set: value => __privateSet"));',
  "    }",
  "",
  "    #[test]",
  "    fn generated_runtime_contains_core_helpers() {",
  "        let source = source(JsFeature::NONE);",
  "        let text = String::from_utf8_lossy(&source.contents);",
  '        assert!(text.contains("export var __pow = Math.pow"));',
  '        assert!(text.contains("export var __commonJS"));',
  '        assert!(text.contains("export var __toESM"));',
  '        assert!(text.contains("export var __async"));',
  '        assert!(text.contains("export var __decoratorStart"));',
  "    }",
  "}",
  "",
);

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.writeFileSync(outputPath, `${lines.join("\n")}`);
