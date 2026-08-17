# Pinned upstream test corpus

Generated fixtures in this directory are extracted from the esbuild revision
recorded in [`UPSTREAM.md`](../../UPSTREAM.md). The generators must reject any
upstream case they cannot translate so missing coverage cannot be silent.

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

The CSS parser generator instruments a temporary copy of upstream's Go test
package and runs it to capture all 2,781 concrete cases, including cases built
by loops and `fmt.Sprintf`. All 139 prefix-insertion cases are active and use
the same all-browser version-zero prefix map as upstream's test helper.

The six checked-in corpora contain 4,291 concrete cases in total:

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

The active bundler tranche exact-compares 750 Unix snapshots and 29
diagnostic-only cases from the default, DCE, import-star, TypeScript import-star,
lowering, TypeScript, package-json,
tsconfig, loader, CSS, code-splitting, Yarn PnP, and entry-point glob suites that use
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
also covered. So 5,071 concrete upstream cases are currently active in
`cargo test`; the remaining 291 captured bundler cases are the parity backlog,
not claimed as passing coverage.

The generated JSON is checked in so `cargo test` does not require Go or a
separate upstream checkout.
