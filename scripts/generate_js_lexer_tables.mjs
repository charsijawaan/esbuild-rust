#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [lexerPath, tablesPath, outputPath] = process.argv.slice(2);
if (!lexerPath || !tablesPath || !outputPath) {
  throw new Error(
    "usage: generate_js_lexer_tables.mjs <js_lexer.go> <tables.go> <tables.rs>",
  );
}

const lexer = fs.readFileSync(lexerPath, "utf8");
const tables = fs.readFileSync(tablesPath, "utf8");

const tokenBlock = lexer.match(
  /const \(\n([\s\S]*?)\n\)\n\nfunc \(t T\) IsAssign/,
)?.[1];
if (!tokenBlock) {
  throw new Error("could not locate token constants");
}

const tokens = [];
for (const line of tokenBlock.split("\n")) {
  const match = line.match(/^\s*(T[A-Za-z0-9]+)/);
  if (match) tokens.push(match[1]);
}

const keywordBlock = lexer.match(
  /var Keywords = map\[string\]T\{\n([\s\S]*?)\n\}/,
)?.[1];
if (!keywordBlock) {
  throw new Error("could not locate keyword table");
}

const keywords = [];
for (const match of keywordBlock.matchAll(/^\s*"([^"]+)":\s+(T\w+),/gm)) {
  keywords.push([match[1], match[2]]);
}

const tokenStringBlock = tables.match(
  /var tokenToString = map\[T\]string\{\n([\s\S]*?)\n\}/,
)?.[1];
if (!tokenStringBlock) {
  throw new Error("could not locate token string table");
}

const tokenStrings = new Map();
for (const match of tokenStringBlock.matchAll(
  /^\s*(T\w+):\s+("(?:[^"\\]|\\.)*"),/gm,
)) {
  tokenStrings.set(match[1], JSON.parse(match[2]));
}

const entityBlock = tables.match(
  /var jsxEntity = map\[string\]rune\{\n([\s\S]*?)\n\}/,
)?.[1];
if (!entityBlock) {
  throw new Error("could not locate JSX entity table");
}

const entities = [];
for (const match of entityBlock.matchAll(
  /^\s*"([^"]+)":\s+0x([0-9A-Fa-f]+),/gm,
)) {
  entities.push([match[1], Number.parseInt(match[2], 16)]);
}

if (
  tokens.length < 100 ||
  keywords.length !== 36 ||
  tokenStrings.size !== tokens.length ||
  entities.length < 250
) {
  throw new Error(
    `incomplete extraction: ${tokens.length} tokens, ${keywords.length} keywords, ` +
      `${tokenStrings.size} token strings, ${entities.length} entities`,
  );
}

const rustName = (goName) => goName.slice(1);
const rustString = (value) => JSON.stringify(value);

const lines = [
  "//! Generated from the pinned upstream `internal/js_lexer` tables.",
  "#![allow(clippy::match_same_arms, clippy::too_many_lines)]",
  "",
  "#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]",
  "#[repr(u8)]",
  "pub enum Token {",
];

for (const [index, token] of tokens.entries()) {
  if (index === 0) lines.push("    #[default]");
  lines.push(`    ${rustName(token)},`);
}

lines.push(
  "}",
  "",
  "pub type T = Token;",
  "",
  "impl Token {",
  "    #[must_use]",
  "    pub const fn is_assign(self) -> bool {",
  "        self as u8 >= Self::AmpersandAmpersandEquals as u8",
  "            && self as u8 <= Self::SlashEquals as u8",
  "    }",
  "",
  "    #[must_use]",
  "    pub const fn as_str(self) -> &'static str {",
  "        match self {",
);
for (const token of tokens) {
  lines.push(
    `            Self::${rustName(token)} => ${rustString(tokenStrings.get(token))},`,
  );
}
lines.push(
  "        }",
  "    }",
  "}",
  "",
  "#[must_use]",
  "pub const fn token_to_string(token: Token) -> &'static str {",
  "    token.as_str()",
  "}",
  "",
  "#[must_use]",
  "pub fn keyword_token(text: &str) -> Option<Token> {",
  "    Some(match text {",
);
for (const [text, token] of keywords) {
  lines.push(`        ${rustString(text)} => Token::${rustName(token)},`);
}
lines.push(
  "        _ => return None,",
  "    })",
  "}",
  "",
  "#[must_use]",
  "pub fn jsx_entity(name: &str) -> Option<char> {",
  "    Some(match name {",
);
for (const [name, value] of entities) {
  lines.push(`        ${rustString(name)} => '\\u{${value.toString(16).toUpperCase()}}',`);
}
lines.push(
  "        _ => return None,",
  "    })",
  "}",
  "",
  `pub const TOKEN_COUNT: usize = ${tokens.length};`,
  `pub const JSX_ENTITY_COUNT: usize = ${entities.length};`,
  "",
);

fs.mkdirSync(path.dirname(outputPath), { recursive: true });
fs.writeFileSync(outputPath, lines.join("\n"));
