# Experimental porting status

This is an experimental, AI-generated Rust port of esbuild. It is incomplete,
unaudited, and not an official esbuild project.

Status reviewed on 2026-10-04 through JavaScript plugins, contexts, cancellation,
and watch in the native service. All comparisons target
`6ff1d8b0d8c134e867a397eef39702a223ebef9e` (esbuild 0.28.1), as recorded in
[UPSTREAM.md](UPSTREAM.md). Separate serving and compiler proposals are excluded
until verified and committed.

## What the percentages mean

- **Captured fixtures: 88.31% active** (12,397 / 14,038). These cases compare
  original upstream output or diagnostics exactly in the normal test suite.
  This measures the captured corpus, not all upstream behavior.
- **Original CLI/runtime suite: 99.93% passing** (1,461 / 1,462), matching
  pinned Go's results on Node v24.15.0, macOS arm64. Both fail case 80; there
  are no Rust-only failures or timeouts in this report. This is a separate
  runtime/platform measurement, not fixture coverage or product completion.
- **Whole-product completion: no fresh percentage established.** The roughly
  **55% whole-product / 75% selected native core** figures in
  [EVALUATION.md](EVALUATION.md) are historical engineering estimates from
  2026-07-31, not current measured completion. Native functionality has advanced,
  but the committed evidence supplies no defensible product-wide denominator
  or weighting with which to replace those estimates.

Do not combine these counting units or infer an overall percentage from them.
The current assessment is a substantial native compiler/CLI implementation with
an explicit conformance backlog and major host/distribution surfaces still
outside the port.

## Measured coverage and remaining suites

| Captured corpus | Active | Remaining inactive | Captured |
| --- | ---: | ---: | ---: |
| Lexer/printer/JSON/CSS parser corpora | 4,291 | 0 | 4,291 |
| JS/TS parser and lowering | 7,128 | 1,515 | 8,643 |
| Bundler | 945 | 126 | 1,071 |
| Go API formatting/directory-prefix helpers | 33 | 0 | 33 |
| Total | 12,397 | 1,641 | 14,038 |

The parser backlog comprises 867 cases from `js_parser_test.go`, 555 from
`ts_parser_test.go`, and 93 from `js_parser_lower_test.go`, calculated from
[the captured cases](tests/upstream/js_parser.json) and
[the committed active indices](tests/upstream/js_parser_active.json).
Inactive means not accepted as passing coverage; it can include output or
diagnostic mismatches, unsupported harness options/filesystems, and candidates
awaiting review. It does not mean one missing feature per case.

[The complete API lowering runtime report](tests/upstream/end_to_end_api_lowering_checkpoint.json)
records the Rust binary from `02ef9cf`, both binary hashes, the environment,
and the shared case-80 failure. Rust and Go have the same pass/fail outcome for
all 1,462 registered cases in that run. Other environments remain unverified.

[The source-level inventory](tests/upstream/test_inventory.json) separately
records 1,271 JavaScript API registrations, 97 plugin registrations, and 13 WASM
registrations. The original Node wrapper passes 59 selected core, plugin,
context, cancellation, and watch registrations in both worker modes through
the native service. The original binary-stdin build also passes. This bounded
selection does not establish the wider suites
as passing. Seven Go utility test files still require a correspondence audit
(`compat`, `fs`, `helpers/dataurl`, `js_ast`, `logger`, `resolver/yarnpnp`, and
`runtime`). Browser, Deno, Test262, decorator, fuzzer, and other scripts are
listed without a complete case inventory. Do not add registration counts or
Go function counts to the captured-fixture denominator.

## Implemented functionality omitted by the old status

These are implemented and exercised paths, not claims of complete feature or
package parity.

| Area | Current implementation and evidence |
| --- | --- |
| Injection | Native `BuildOptions.inject` and CLI `--inject:FILE` (`02230a5`); unbound/dotted names, shadowing, live bindings, defines, splitting, tree shaking, and rebuilds. Nine bundler cases and four parser cases were activated; runtime cases 269 and 313 now pass. [API tests](tests/injection.rs), [CLI tests](tests/injection_cli.rs). |
| Watch | Native contexts support rebuild/watch/dispose. CLI `--watch`, `--watch=forever`, and `--watch-delay` use that context (`578cdb6`), with stdin lifetime, error recovery, dependency edits, and output updates. The Node service now bridges background rebuilds and plugin acknowledgments; 11 selected original watch functions pass in both worker modes. [CLI implementation](src/cli_watch.rs), [service implementation](src/service/watch.rs), [lifecycle probes](scripts/test_service_watch.mjs). |
| Property mangling | API and CLI expose property patterns, reservations, quoted-property control (`5cbf3c3`), and reusable caches (`40167fe`). API build/transform results return caches; CLI builds persist `--mangle-cache=FILE`. [Mangling tests](tests/property_mangling.rs), [cache tests](tests/mangle_cache.rs). |
| Resource management | `using` / `await using` lowering (`f1b8b91`) handles disposal order, abrupt exits, async disposal, module hoisting, and TypeScript scopes. All five captured resource-management bundler fixtures are active; six original runtime cases were fixed. [Implementation](src/internal/js_parser/lower_using.rs), [tests](tests/using_lowering.rs). |
| Diagnostics | CLI color, log-level filtering, and log overrides are implemented; native build/transform options expose `log_override`. [CLI tests](tests/cli_diagnostics.rs), [API implementation](src/api/mod.rs). Logging parity remains bounded as described below. |
| Defines and target lowering | Native defines support `this`, `import.meta`, and complete property chains before lowering. Non-string dynamic imports lower through deferred `require` calls for unsupported targets, bundled `node:` imports follow target and output-format support, and unsupported RegExp syntax lowers to effectful constructor calls. [Define tests](tests/defines_import_meta.rs), [dynamic import tests](tests/dynamic_import_expressions.rs), [Node prefix tests](tests/node_prefix_targets.rs), [RegExp tests](tests/regexp_feature_lowering.rs). |
| Raw tsconfig | Build-time `tsconfigRaw` paths/baseUrl use the build cwd, replace discovered configs, and exclude dependency imports inside node_modules. Direct transforms keep filesystem inheritance isolated; raw build-time `extends` remains a gap. [Tests](tests/tsconfig_raw_build.rs). |

The latest parser audit activates nine original impossible-`typeof` warning
cases and two identifier-escape diagnostics. The latter also exercise the
previously committed raw-byte lexer quoting. Native API validation now preserves
upstream external/alias error precedence and absolute-path warning behavior.
[Validation tests](tests/api_validation_paths.rs) cover those boundaries.

[JS lowering](src/internal/js_parser/visit.rs) now includes optional chains,
nullish/logical assignment, object spread/rest, async functions and async
generators, `for await`, class fields/private members/static blocks,
auto-accessors, exponentiation assignment, and decorator paths. Executable
[transform regressions](tests/transform_runtime.rs) exercise helpers,
receivers, initialization order, and name/scope behavior. Parser/lowering
mismatches remain; an implemented path is not exhaustive compatibility.
Dynamic `require()` / `import()` glob bundling and entry-point globs also have
active original fixtures.

The old claim that five CSS compatibility features are table-only is obsolete:
[the CSS parser](src/internal/css_parser/mod.rs) uses `color-functions`,
`gradient-interpolation`, `gradient-midpoints`, `is-pseudo-class`, and `nesting`
feature bits in transformation paths. The captured CSS parser corpus is fully
active, while CSS bundler and broader browser behavior still need parity work.

## Package, API, and protocol boundaries

All 25 production upstream `internal` package names have Rust counterparts.
This is a structural mapping, not a package-completeness percentage. The
public surface is a native Rust library and executable, not the upstream
JavaScript or Go API distribution.

| Surface | Current boundary |
| --- | --- |
| `internal/*` compiler packages | Parser/printer, resolver, linker, bundler, runtime helpers, minification, CSS, and source maps are implemented to varying degrees. The fixture backlog prevents a blanket package-parity claim. |
| `cmd/esbuild`, `pkg/cli` | Native CLI includes watch and a framed service for build/transform, formatting, analysis, JavaScript plugins, contexts, cancellation, and watch. Serve remains unsupported; CLI/protocol compatibility is not complete. |
| `pkg/api` | Native Rust build/transform/context APIs, message formatting, metafile analysis, and plugin callbacks. Rebuild/watch/cancel/dispose exist; serve remains unsupported. The 33 captured Go helper cases do not verify the whole API. |
| Plugins | Native setup, resolve/load, lifecycle callbacks, nested resolution, plugin data, and watch paths are bridged to the pinned original Node wrapper. Callback transport and context lifetimes have bounded original and focused tests; the full plugin suite remains unverified. |
| Host/distribution | The native service works with the pinned original Node wrapper in bounded tests. Wrappers are not distributed here; WebAssembly, npm/platform packages, and release/distribution tooling remain unported. |

Remaining native work includes the inactive suites above and wider syntax,
TypeScript/decorator, minifier, resolver, diagnostic, and source-map parity.
Native build/transform options still lack color/log-level/log-limit controls;
CLI `--log-limit`, extra info/debug/verbose diagnostics, and resolver tracing
remain gaps. Unsupported-target guards also reject some generator,
default/rest/destructuring, and ES5 class/`let`/`const` transformations. Such
rejections are limitations, not automatically Rust-only gaps: compare the
original upstream behavior before classifying them.

## Validation limits

The latest recorded normal suite passed **1,257 Rust tests**, many of which
iterate over captured cases; this is not 1,257 additional upstream cases.
The exhaustive parser audit is separately ignored in normal runs. Strict
Clippy remains blocked by **621 previously recorded errors**. See
[tests/upstream/README.md](tests/upstream/README.md) for the committed checks,
fixture selection rules, audit commands, and runtime checkpoint history.

The service integration reran the normal suite and 59 original functions in
both worker modes, plus watcher callback/disposal and framed-transport probes.
Strict Clippy retains the same 621 diagnostic identities; the plugin constructor
was split into private helpers to remove its new length suppression.
Native host disconnect cleanup deliberately wakes blocked callbacks and disposes
retained contexts, while pinned Go can retain these processes after EOF. Raw
duplicate-dispose behavior also differs. Watch build diagnostics are forwarded,
but info/debug/verbose watcher status notices remain absent. Cross-platform filesystem and
runtime coverage, the wider API/plugin/WASM suites, security review, and
performance benchmarks remain incomplete. The older 20-scenario release
matrix in [EVALUATION.md](EVALUATION.md) is historical evidence, not a fresh
release or production-readiness assessment.

## Keeping build caches small

Parallel parity work uses one reusable Cargo target per task. Disable debug
data and incremental caches for these verification builds:

```sh
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test --all-targets --locked
```

After a task finishes, preserve its source, patches, reports, manifests, and
frozen acceptance binaries before removing its stale `target` directory. Avoid
copying target directories into new source snapshots. `cargo clean` removes
generated build output in the selected checkout; do not clean a target while
another task is using it. These settings affect Rust debugging/build caches,
not the JavaScript or CSS emitted by esbuild.
