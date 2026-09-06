# Pinned upstream test corpus

Generated fixtures in this directory are extracted from the esbuild revision
recorded in [`UPSTREAM.md`](../../UPSTREAM.md). The generators must reject any
upstream case they cannot translate so missing coverage cannot be silent.

The captured corpus currently contains 14,005 concrete cases:

| Corpus | Active | Remaining | Captured |
| --- | ---: | ---: | ---: |
| Original lexer/printer/JSON/CSS parser corpora | 4,291 | 0 | 4,291 |
| JS/TS parser and parser lowering | 6,726 | 1,917 | 8,643 |
| Bundler | 893 | 178 | 1,071 |
| Total | 11,910 | 2,095 | 14,005 |

This is coverage of the captured fixtures, not the entire upstream product test
surface. Go utility/API tests and upstream's JavaScript API, plugin, WebAssembly,
and platform matrices still need a complete correspondence inventory.

Regenerate the JavaScript printer corpus with:

```sh
node scripts/generate_upstream_js_printer_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/js_printer.json
```

Regenerate the JavaScript lexer corpus with:

```sh
node scripts/generate_upstream_js_lexer_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/js_lexer.json
```

Regenerate the JSON parser corpus with:

```sh
node scripts/generate_upstream_json_parser_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/json_parser.json
```

Regenerate the CSS printer corpus with:

```sh
node scripts/generate_upstream_css_printer_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/css_printer.json
```

Regenerate the CSS lexer corpus with:

```sh
node scripts/generate_upstream_css_lexer_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/css_lexer.json
```

Regenerate the CSS parser corpus with:

```sh
node scripts/generate_upstream_css_parser_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/css_parser.json
```

Regenerate the bundler corpus with:

```sh
node scripts/generate_upstream_bundler_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/bundler.json
```

Regenerate the JS/TS parser and parser-lowering corpus with:

```sh
node scripts/generate_upstream_js_parser_tests.mjs \
  /path/to/pinned/esbuild \
  tests/upstream/js_parser.json
```

This generator checks the pinned revision, captures all calls through upstream's
two common parser test helpers, and runs their original Go assertions. It captures
6,578 cases from `js_parser_test.go`, 1,606 from `ts_parser_test.go`, and 459 from
`js_parser_lower_test.go`, including generated cases. Source and expected output
are base64-encoded to preserve invalid UTF-8. Go line directives preserve original
source locations despite instrumentation. Option translation rejects unknown
fields instead of silently substituting defaults.

`js_parser_active.json` contains the zero-based indices of the 6,726 exact-matching
cases enforced in normal `cargo test`. The ignored audit test runs all 8,643 cases
and reports mismatches without treating a completed audit as a conformance pass:

```sh
ESBUILD_RS_UPSTREAM_PARSER_REPORT=/tmp/parser-audit.json cargo test --lib \
  audits_pinned_upstream_js_parser_corpus -- --ignored --nocapture
```

Use `ESBUILD_RS_UPSTREAM_PARSER_INDEX` to select an individual fixture or
`ESBUILD_RS_UPSTREAM_PARSER_TEST` to select an upstream test group (including its
inactive cases) with `matches_pinned_upstream_active_js_parser_corpus`.

The CSS parser generator instruments a temporary copy of upstream's Go test
package and runs it to capture all 2,781 concrete cases, including cases built
by loops and `fmt.Sprintf`. All 139 prefix-insertion cases are active and use
the same all-browser version-zero prefix map as upstream's test helper.

The original six corpora contain 4,291 concrete cases in total:

- JavaScript printer: 695
- JavaScript lexer: 335
- JSON parser: 122
- CSS printer: 289
- CSS lexer: 69
- CSS parser: 2,781

The bundler generator runtime-instruments upstream's Go test package so cases
constructed by loops and helpers are also captured. The checked-in fixture has
1,071 calls across all 14 upstream bundler suites: 955 output snapshots and 116
diagnostic-only cases. It records unsupported non-serializable option fields
instead of silently dropping them (`Defines` in 13 cases and `Plugins` in one).
Files containing invalid UTF-8 are additionally stored in a base64 sidecar so
binary-loader snapshots retain the exact upstream bytes.

The active bundler tranche exact-compares 815 Unix snapshots and 77
diagnostic-only cases from the default, DCE, import-star, TypeScript import-star,
lowering, TypeScript, package-json,
tsconfig, loader, CSS, code-splitting, Yarn PnP, import-phase, and entry-point glob suites that use
`AbsOutputFile` or `AbsOutputDir`, with all eligible DCE cases and selected
combinations of `Mode`, `OutputFormat`, `Platform`, legal-comment settings,
`KeepNames`, `MinifySyntax`, `MinifyIdentifiers`, `MinifyWhitespace`, and
`TreeShaking`. This includes pre- and post-resolution external matching,
conditional `require()`, `require.resolve()`, and `import()` paths, external
namespace imports and re-exports, import-assertion comment preservation, and
automatic JSX runtime settings from tsconfig. Explicit JSX factory, fragment,
automatic-runtime, development, side-effect, import-source, and preserve settings
are covered too, including generated-import name collisions, top-level `this`
rewrites, and comments attached to preserved JSX expression containers. It also
checks lowering for async functions and arrows, async and static `super`
property access, private fields and methods, private optional chains and brand
checks, public class fields, class-expression captures, nullish and logical
assignment, `export * as` lowering, and `for await` iteration. It also
includes dynamic `require()` and `import()` glob modules in
both single-file and code-splitting builds, glob import attributes, empty-glob
warnings, entry-point glob expansion, and advanced entry points with explicit
output paths. Package resolution coverage includes configured main-field
priority, dual-package selection, and package self-references through exports
maps, custom export conditions, package aliases, external-package ordering,
and absolute node search paths with browser remapping. It also checks
output-base and public-path asset layouts plus
metafile input/output accounting for JS, CSS, JSON attributes, copied files,
and code-split long paths. The missing-glob-directory diagnostic-only case is
also covered. All 12 import-phase cases are active, including external glob
imports with distinct attributes. CSS import diagnostics validate loader
compatibility, missing or global `composes` names, output paths, and external
patterns with query/hash suffixes. The harness now translates extension order,
property-mangling controls, output names, banners, drop labels, source maps,
CSS targets, and explicit tsconfig paths, and rejects unmapped options even
when selecting an inactive case. `bundler_additional_active.json` enables 67
reviewed cases beyond the original option-based selection. So 11,910 concrete
upstream cases are currently active in `cargo test`; the remaining 178 captured bundler cases are the parity backlog,
not claimed as passing coverage.

List active and inactive bundler fixtures, including their filesystem variant:

```sh
ESBUILD_RS_UPSTREAM_LIST=1 cargo test --lib \
  matches_pinned_upstream_active_bundler_corpus -- --nocapture
```

Audit inactive fixtures without enabling them or changing expected output:

```sh
node scripts/audit_upstream_bundler_tests.mjs /tmp/bundler-audit.json
# Optionally limit the audit to a suite:
node scripts/audit_upstream_bundler_tests.mjs /tmp/css-audit.json css
```

The audit reports passing candidates, failures, unsupported options/filesystems,
and the separately covered missing-glob test. A passing candidate still needs
review of the option translation before it can be added to active coverage.
The parser backlog is tracked separately in the table above.

The generated JSON is checked in so `cargo test` does not require Go or a
separate upstream checkout.
