# Pinned upstream test corpus

Generated fixtures in this directory are extracted from the esbuild revision
recorded in [`UPSTREAM.md`](../../UPSTREAM.md). The generators must reject any
upstream case they cannot translate so missing coverage cannot be silent.

The captured corpus currently contains 14,038 concrete cases:

| Corpus | Active | Remaining | Captured |
| --- | ---: | ---: | ---: |
| Original lexer/printer/JSON/CSS parser corpora | 4,291 | 0 | 4,291 |
| JS/TS parser and parser lowering | 7,056 | 1,587 | 8,643 |
| Bundler | 918 | 153 | 1,071 |
| Go API formatting and directory-prefix helpers | 33 | 0 | 33 |
| Total | 12,298 | 1,740 | 14,038 |

This is coverage of the captured fixtures, not the entire upstream product test
surface. Go utility tests and upstream's JavaScript API, plugin, WebAssembly,
and platform matrices still need a complete correspondence inventory.

## Repository-wide inventory

`test_inventory.json` records source-level accounting for the pinned revision:
33 Go test files with 1,392 test functions, plus 1,271 registered JavaScript API
tests (537 build, 686 transform, and 48 in other groups), 97 plugin tests, and 13
WASM tests. JavaScript registration is inspected without invoking the suite's
entry point, installing a package, or executing a test. These registrations are
**not** claimed as passing Rust coverage. Go utility files without captured
fixtures and other scripts (including Test262 and fuzzers) are listed explicitly
for further correspondence work.

Function definitions, generated helper cases, and JavaScript registrations are
different counting units. Do not add these totals into one parity denominator.

```sh
node scripts/audit_upstream_test_inventory.mjs /path/to/pinned/esbuild tests/upstream/test_inventory.json
```

## CLI and runtime end-to-end suite

The original `scripts/end-to-end-tests.js` can also run directly against either
binary, without rewriting its assertions or generated programs:

```sh
cargo build --bin esbuild
node scripts/audit_upstream_end_to_end_tests.mjs /path/to/pinned/esbuild \
  target/debug/esbuild /tmp/rust-e2e.json
node scripts/audit_upstream_end_to_end_tests.mjs /path/to/pinned/esbuild \
  /path/to/pinned/esbuild/esbuild /tmp/go-e2e.json
```

This suite registers 1,462 tests on Node 24/macOS; some registrations depend on
the runtime/platform and some tests exercise multiple output formats. These are
separate from the 14,038 fixture cases above. The runner reads the original
committed test script, substitutes the compiler executable, and gives each test
an isolated temporary directory and process group. It retains failed artifacts,
writes an incremental JSON report, and returns failure if any selected test fails
or times out. Process groups are killed on timeout or interruption.

Use `ESBUILD_RS_E2E_START` and `ESBUILD_RS_E2E_LIMIT` to select a contiguous range,
or `ESBUILD_RS_E2E_TIMEOUT_MS` to adjust the 90-second per-test timeout. Executable
startup has a separate preflight check. Reports include Node/platform details;
compare with the pinned Go binary before attributing a failure to the port.

The initial complete run at Rust commit `7f5bcce` on Node 24.15.0/macOS is recorded
in `end_to_end_baseline.json`: Rust passed 1,214, failed 247, and timed out once;
Go passed 1,461 and failed once. Case 80 fails with the same native async-generator
behavior on both binaries. The remaining 247 Rust-only failures/timeouts are a
separate runtime parity backlog, not included in the fixture table. No upstream
assertion was changed to accept these results. Case 1415 (`--analyze` on a
stdin-only transform) times out at that baseline because the Rust CLI waits for
stdin instead of validating the flag combination first.

Subsequent checkpoints fixed case 1415 (verified individually), inherited
tsconfig paths through symlinks (case 25), and browser-field lookup of bare file
keys and implicit extensions (cases 31, 33, 35, and 43). The first 50 original
runtime cases now pass 50/50 after fixing JSX diagnostic cases 26 and 27. The
initial baseline above is retained as a historical measurement. Local
regressions also cover `preserve_symlinks` and disabled browser mappings, which
must preserve the requested path without requiring the original file to exist.

The complete comparison at Rust commit `0473d7c` is recorded separately in
`end_to_end_checkpoint.json`: Rust passed 1,258, failed 204, and had no timeouts;
Go again passed 1,461 with the same shared failure at case 80. There are 203
Rust-only failures, 44 fixes since the initial baseline, and no regressions.
The Rust executable was snapshotted before the run to prevent later builds from
mixing compiler versions within the report.

This checkpoint includes CommonJS `module.require()` dependency recognition,
async-generator lowering, direct-eval diagnostics, re-exported TypeScript import
retention, external ESM namespace aliases, and missing-import warnings with
`undefined` substitution. The API no longer turns warning-only linker issues
into generic errors. Warning suppression and promotion preserve output/exit
behavior and source locations. All eight formerly inactive import-star cases
now match their original output and diagnostics exactly.

The next verified checkpoint moves potentially throwing async parameter
initializers and destructuring into the promise wrapper, preserving function
length and forwarded arguments. Parser case 115 (`TestLowerAsyncFunctions`)
now matches upstream exactly, bringing active fixture coverage to 12,247.
The original runtime slice 1250–1266 passes 16/17: case 1266 is newly fixed;
case 1254 is an existing class/super receiver failure. This is a targeted
verification, not a replacement for the complete 1,258-pass report above.
The local suite passes 906 library, 42 CLI, and four integration tests (952
total; the exhaustive parser audit is separately ignored in normal runs).

Default-format JavaScript, JSX, TypeScript, and TSX transforms now use the
build linker to include all lowering helpers and their transitive dependencies.
The former direct printer path and its partial helper definitions were removed.
Comparison with pinned Go confirmed its surrogate-pair string escaping and
the existing TypeScript namespace minifier output. The linker now keeps
namespace-local declaration counts separate from property-access use counts;
imported namespace aliases still count toward the imported namespace.

Five executable regressions in `tests/transform_runtime.rs` exercise public API
and stdin transforms with Node: async parameter rejection, function length,
lexical `this`/`arguments`, helper-name collisions, transitive async-generator,
private-field and spread/rest helpers, minification, source-map stack locations,
and extracted legal comments. A separate regression for a parameter named
`arguments` checks Rust behavior; the earlier pinned-Go panic for that spelling
is not treated as a reference output. The native async target also exposed a
missing separator in minified `return async (...) => ...`, which is corrected.
The normal local suite now passes 957 tests (906 library, 42 CLI, and nine
integration). Strict Clippy remains blocked by pre-existing repository lints,
verified separately against untouched `965fee5`; no lint suppression was added.

The complete runtime audit at `10ddb1a` is recorded separately in
`end_to_end_transform_checkpoint.json`: Rust passed 1,259 of 1,462, failed 203,
and had no timeouts; Go passed 1,461 with its same failure at case 80. Relative
to `0473d7c`, case 1266 is newly passing and there are no regressions. The
remaining 202 Rust-only runtime failures are separate from the captured fixture
backlog of 1,629 parser/lowering and 162 bundler cases (1,791 total). The wider
API/plugin/WASM inventory remains separate.

Private calls and tagged templates preserve their receivers while lowering,
including function-valued private fields and getters. The original runtime
slice 1250–1266 now passes 17/17, fixing case 1254 without changing its assertion.
An executable regression checks method/field/getter calls and tags, static
members in class expressions, receiver mutation, single receiver evaluation,
and parameter-scope captures across ES2015/ES2017/ES2022 and minification.
The normal suite passes 958 tests. A full parser audit preserves all 7,014 active
cases; inactive exact matches still need individual review before activation.
Converted named class declarations now retain their inner class value for
generated static initialization even when user code does not reference it.
Static private brands/fields and public static fields initialize that captured
value before assigning the outer class binding. An executable regression checks
initialization order, private methods/fields/getters used as calls and tags,
outer-binding reassignment, derived classes, and static private brand checks,
with and without minification, with keep-names, and across native/lowered targets.
The normal suite passes 959 tests. Original runtime case 904 is newly passing
in the targeted 900–917 slice, which has no regressions. The complete runtime
audit will establish the combined effect of the receiver and class-capture fixes.

The first combined audit exposed two regressions in bundled computed static
fields (original runtime cases 1090 and 1183). Class lowering now preserves an
inner class reference for static initialization only when it actually captures
the class value outside the expression. Ordinary bundled public static fields
continue to initialize the outer binding. Both original cases pass again, and
an executable public-build-API regression checks evaluation order and the field
value with ES2015/ES2022 targets and minification. Execution fixtures also use
process and sequence identifiers to avoid temporary-file collisions when tests
run concurrently. The normal suite passes 960 tests.

The complete runtime audit at `b3ffd98` passes 1,271 of 1,462 cases with no
timeouts: 12 gains and no regressions relative to `10ddb1a`. The same pinned Go
reference still fails only case 80. Private method writes, getter-only writes,
and setter-only reads now emit the original `private-name-will-throw` warning,
with member locations and log overrides. Dependency files use debug-level
diagnostics by default, while transform sourcefile labels retain warnings,
matching pinned Go. Instance/static members, compound assignments and updates,
accessor pairs, and try/catch bodies have local diagnostic regressions.
The four original runtime cases 838, 931, 1022, and 1115 pass individually.
Six original `TestPrivateIdentifiers` diagnostic cases (6564–6569) now match
exactly and are active; the full parser audit preserves all prior active cases.
The normal suite passes 962 tests.

The complete runtime audit at `f606229` is preserved separately in
`end_to_end_private_checkpoint.json`: 1,275 passed, 187 failed, and no timeouts.
It has 16 newly passing cases and no regressions relative to `10ddb1a`; four
of the gains come from the private-access warnings. Go still passes 1,461,
with the shared case-80 failure. The 186 Rust-only runtime failures remain
separate from the 1,785 inactive fixture cases and the API/plugin/WASM inventory.

Duplicate class members now use `duplicate-class-member` and the original
member wording; object keys retain `duplicate-object-key`. Both warning and
note underline the identifier range. Checks are skipped for dependency files,
matching upstream even when a log override promotes these messages to errors.
Ten original `TestWarningDuplicateClassMember` diagnostic cases now match and
are active. The full parser audit preserves every prior active case, and the
normal suite passes 964 tests. Original runtime slices 838–840, 931–933,
1022–1024, and 1115–1117 pass 12/12, fixing eight duplicate-member cases.

Constant-assignment diagnostics retain the identifier's source name before
symbol resolution, so class warnings and notes report `Foo` instead of the
generated `_Foo`. Local API regressions cover native/lowered targets,
minification, escaped identifiers, warning locations, and bundle-mode errors.
Original runtime cases 851–852, 944–945, 1035–1036, and 1128–1129 pass 8/8.
The normal suite passes 965 tests and the full parser audit preserves all
7,030 active cases. Class visitation uses named options for its lowering flags;
strict Clippy still has the same 625 pre-existing error diagnostics as untouched
`965fee5`, with no new diagnostic categories or lint suppression.

The combined runtime audit at `a764bd0` passes 1,291 of 1,462, with 16 gains
and no regressions relative to `f606229` (eight duplicate-member and eight
source-name diagnostic cases). Lowered expression-only static blocks now join
static field initializers in source order, after helper storage and private
brands are initialized. Converted classes assign their outer binding after
this initialization. The block rewrite follows lexical arrows, their parameter
defaults/bindings, and nested control flow, while ordinary functions and class
bodies retain their own receivers. Existing field-arrow snapshots remain
unchanged. A new executable regression passes 24 declaration/expression,
transform/bundle, target, and minification combinations on Rust and pinned Go.
Original runtime cases 969 and 1153 pass individually. Seven original
`TestLowerClassStaticBlocks` cases (271–277) now match exactly and are active.
The normal suite passes 966 tests; the full parser audit preserves all prior
active cases. Strict Clippy has 623 pre-existing error diagnostics, with two
removed by the helper refactor and none added relative to the baseline.

The full runtime audit at `85f9b74` is recorded in
`end_to_end_class_checkpoint.json`: 1,293 passed, 169 failed, and no timeouts.
Relative to `f606229`, 18 cases are newly passing and none regressed. Go still
passes 1,461 with the shared case-80 failure, leaving 168 Rust-only runtime
failures alongside the separate 1,768 inactive fixture cases.

Disabling `class-static-blocks` now also lowers the surrounding static fields
when field syntax itself remains supported. All private members reachable from
the moved initializers are marked for lowering, and class-lowering decisions
honor `PRIVATE_SYMBOL_MUST_BE_LOWERED`. This preserves private scope for static
method/field access, instance getters, and brand checks. The executable
initialization regression now passes 32 combinations, including an ES2022
static-block override, on Rust and pinned Go. The local suite remains at 966
passing tests and the full parser audit at 7,037 matches, with no active
regressions or new inactive matches. Strict Clippy adds no diagnostics relative
to the 623-error checkpoint.

TypeScript assignment-style fields now capture dynamic computed keys during
class definition, preserving their order relative to methods, omitted fields,
and the base-class expression. Literal keys retain their existing output.
Lowered static assignments use the captured class value for both declarations
and expressions. The executable regression passes 16 combinations of targets,
minification, transform/bundle modes, and a static-block override on Rust and
pinned Go. Three exact upstream cases (`TestTSClass`, `TestTSSuperCall`, and
`TestTSClassSideEffectOrder`) are now active: the full parser audit matches
7,040 cases with no active regressions, and the local suite passes 967 tests.
The complete runtime audit of the preceding static-block override checkpoint
remained at 1,293 passing cases, with no gains or regressions.

The full runtime audit at `5127194` is recorded in
`end_to_end_ts_computed_checkpoint.json`: 1,297 passed, 165 failed, and no timeouts.
Cases 877, 970, 1061, and 1154 are newly passing relative to `c2fb0ab`, with no
regressions. The pinned Go result remains 1,461 passed with the shared case-80
failure, leaving 164 Rust-only runtime failures.

Private optional chains now preserve each null check and the receiver of
lowered method, function-field, and getter calls. Their captures remain in
scope across parameter-default wrappers, and parenthesized accesses retain
their throwing behavior. Original runtime cases 924 and 1108 pass individually.
A new executable regression passes native Node execution and 12 Rust target,
minification, transform/bundle, and private-feature override combinations.
The receiver-mutation and nested-parameter portions also cover known failures
in pinned Go and are not counted as new upstream coverage. The normal suite
passes 968 tests; the full parser audit retains 7,040 matches without active
regressions or new inactive matches. Strict Clippy retains 623 existing errors
without added diagnostics.

The full runtime audit at `9ae1ea0` is recorded in
`end_to_end_private_optional_checkpoint.json`: 1,299 passed, 163 failed, and no
timeouts. Cases 924 and 1108 are newly passing relative to `5127194`, with no
regressions. Excluding the shared case-80 failure leaves 162 Rust-only failures.

Named declarations with lowered private members now capture their inner class
binding outside the class expression. Extracted getters, setters, and methods
can use that binding even after the outer name is reassigned. With `keep_names`,
the source name is restored before static initialization; generated name blocks
do not consume a parsed static-block scope. A new executable regression passes
16 transform/bundle, target, minification, and feature-override combinations on
Rust and pinned Go, including compound assignments whose getter changes the
receiver variable. The local suite passes 969 tests and the full parser audit
retains 7,040 matches without active regressions or new inactive matches.
Strict Clippy retains 623 existing errors without added diagnostics.

The full runtime audit at `26328d6` is recorded in
`end_to_end_private_binding_checkpoint.json`: 1,303 passed, 159 failed, and no
timeouts. Cases 792, 1106, 1107, and 1110 are newly passing relative to `9ae1ea0`,
with no regressions. Excluding the shared case-80 failure leaves 158 Rust-only
failures.

Private storage and extracted member functions inside a factory now belong to
that factory's function scope. Repeated calls keep distinct fields, static
storage, accessors, method closures, and brands. Module-level allocation retains
its existing naming. A new executable regression passes native Node execution
and 12 target, minification, transform/bundle, and feature-override combinations
on Rust and pinned Go, including declaration, expression, and arrow factories
and generated-name collisions. The normal suite passes 970 tests; the parser
audit retains 7,040 matches with no active regressions or new inactive matches.
Strict Clippy retains 623 existing errors without added diagnostics.

The complete factory-storage runtime audit at `ed98b35` remains at 1,303 passes,
with no gains, regressions, or timeouts relative to `26328d6`.

Named class expressions now share their captured binding with moved static
initializers and extracted private members, and private storage names use the
inner class name when present. A new executable regression passes native Node
execution and 12 Rust/pinned-Go combinations covering repeated factories,
instance/static private members, public static initializers, minification,
feature overrides, and `keep_names`. Original `TestLowerClassStatic` case 215
now matches exactly and is active. The normal suite passes 971 tests; the full
parser audit matches 7,041 cases with no active regressions. Strict Clippy
retains 623 existing errors without added diagnostics.

The full runtime audit at `b8288a0` remains at 1,303 passes, with no gains,
regressions, or timeouts relative to `ed98b35`.

Undecorated and legacy-decorated auto-accessors now lower to private backing
fields and getter/setter methods when required by the target or feature
overrides. Generated setter arguments have their own scope, computed keys are
captured once, and mixed public/private field initializers retain source
order. A new executable regression covers private accessors, repeated
factories, symbols, descriptors, derived constructors, static `this`/`super`,
anonymous classes, and minification in 20 Rust transform/bundle combinations.
The same source passes 10 pinned-Go transform combinations. All 14 captured
`TestLowerAutoAccessors` fixtures now match exactly and are active, bringing
the full parser audit to 7,055 matches without active regressions.
The normal suite passes 972 tests. Strict Clippy retains 623 existing errors
without added diagnostics.

The complete runtime audit at `738873b` is recorded in
`end_to_end_auto_accessor_checkpoint.json`: 1,328 passed, 134 failed, and no
timeouts. It gains 25 original cases without regressions relative to
`b8288a0`. The pinned Go executable still passes 1,461 tests; case 80 remains
the shared environment failure, leaving 133 Rust-only failures.

The Rust build/transform API and CLI now expose property mangling patterns,
reserved property patterns, and quoted-property mangling. Compilation applies
one property-name map across modules and split chunks while preserving local
identifier renaming. Stored property names resolve during visiting, and
lowered TypeScript assignment fields keep the mangled key. Patterns use the
existing Rust regex engine; invalid patterns report errors through the API
and CLI. Executable regressions cover 24 Rust API/CLI combinations, two
TypeScript targets, and four module/splitting combinations. The main source
also passes 16 pinned-Go transform/bundle combinations. Nine original
property-mangling/reserved-property bundler snapshots now match exactly and
are active.
The normal suite passes 976 tests. The parser audit retains 7,055 matches
without active regressions, and strict Clippy retains 623 existing errors
without added diagnostics.

The complete runtime audit at `5cbf3c3` is recorded in
`end_to_end_property_mangling_checkpoint.json`: 1,337 passed, 125 failed, and
no timeouts. It gains all nine original cases using `--mangle-props` without
regressions relative to `738873b`. Case 80 remains the shared environment
failure, leaving 124 Rust-only failures.

Class names required by `keep_names` are now prepared before class lowering,
so static fields and blocks observe the source name. This covers declarations,
named/inferred expressions, assignments, object/class members, destructuring
defaults, and default exports. Generated name blocks participate in existing
static initialization ordering, and default exports avoid adding a second
name block afterward. Classes with an explicit static `name` field retain a
captured binding for moved initializers. A new regression passes native Node
execution and 12 Rust/pinned-Go transform/bundle combinations covering older
targets, feature overrides, and minification. The normal suite passes 977
tests, the parser audit retains 7,055 matches without active regressions, and
strict Clippy retains 623 existing errors without added diagnostics.

The complete runtime audit at `c403793` is recorded in
`end_to_end_class_name_checkpoint.json`: 1,360 passed, 102 failed, and no
timeouts. It gains 23 original cases without regressions relative to
`5cbf3c3`. Case 80 remains the shared environment failure, leaving 101
Rust-only failures.

TypeScript `export =` now lowers to an assignment using the CommonJS module
symbol, so local bindings named `module` remain distinct from renamed wrapper
parameters. Export assignments also move after other statements when tree
shaking is disabled. An executable regression covers seven sources with
shadowed bindings, source scopes, declaration ordering, minification, and
CommonJS/ESM/IIFE output through 70 Rust API/CLI combinations. These sources
also pass 56 pinned-Go transform/bundle combinations. The original
parser ordering fixture now matches exactly and is active. The normal suite
passes 978 tests; the parser audit has 7,056 matches without active regressions,
and strict Clippy retains 623 existing errors without added diagnostics.

The complete runtime audit at `3a5e782` is recorded in
`end_to_end_export_equals_checkpoint.json`: 1,361 passed, 101 failed, and no
timeouts. Original case 818 is newly passing without regressions relative to
`c403793`. Case 80 remains the shared environment failure, leaving 100
Rust-only failures.

Name preservation now follows the surrounding binding/property context.
Property assignments keep anonymous function/class names empty, computed
object keys keep their runtime names, and destructuring assignment defaults
use the binding name instead of the property alias. Lowered private methods
and accessors receive their original private name before assignment to the
generated storage binding. The executable regression passes native Node and
eight Rust/pinned-Go transform/bundle combinations, including feature
overrides and minification. The normal suite passes 979 tests; the parser
audit retains 7,056 matches without active regressions, and strict Clippy
retains 623 existing errors without added diagnostics.

The complete runtime audit at `e3369d7` is recorded in
`end_to_end_inferred_names_checkpoint.json`: 1,369 passed, 93 failed, and no
timeouts. Eight original cases are newly passing without regressions relative
to `3a5e782`. Case 80 remains the shared environment failure, leaving 92
Rust-only failures.

Static field lowering now lowers every private member before visiting moved
initializers, including private features still supported by the target. This
avoids private-member syntax escaping its class scope. Static blocks move with
lowered fields and retain source order even when static blocks are supported.
The blanket decision precedes file-wide brand-check flags, preserving the
pinned Go brand-check snapshot. An executable regression passes native Node
and 24 Rust/pinned-Go transform/bundle combinations across six feature
overrides, minification, and repeated factory calls. The normal suite passes
980 tests; the parser audit retains 7,056 matches without active regressions,
and strict Clippy retains 623 existing errors without added diagnostics.

The complete runtime audit at `fea5233` is recorded in
`end_to_end_static_private_checkpoint.json`: it retains 1,369 passes, 93
failures, and no timeouts, with no gains or regressions relative to `e3369d7`.
Case 80 remains the shared environment failure, leaving 92 Rust-only failures.

The printer restores computed brackets for folded negative numeric keys and
minified positive infinity, where ordinary property syntax would be invalid.
An executable regression covers object properties, class fields/methods,
destructuring, signed zero, non-finite values, exponent notation, and large
integers. It passes native Node and four Rust/pinned-Go transform/bundle
combinations. The normal suite passes 981 tests; the parser audit retains
7,056 matches without active regressions, and strict Clippy retains 623
existing errors without added diagnostics.

The complete runtime audit at `b1cc6c4` is recorded in
`end_to_end_numeric_keys_checkpoint.json`: 1,371 passed, 91 failed, and no
timeouts. Original cases 560 and 561 are newly passing without regressions
relative to `fea5233`. Case 80 remains the shared environment failure,
leaving 90 Rust-only failures.

The CLI supports diagnostic filtering with `--log-level`, including suppressing
the summary below `info` and keeping a failing exit status in `silent` mode.
Per-message `--log-override:MESSAGE=LEVEL` settings are passed through to the
logger for builds and transforms, including warning suppression, promotion to
errors, and grouped `package.json`/`tsconfig.json` IDs. Unknown IDs are ignored,
matching upstream. The Rust API exposes the corresponding `log_override` map.
Extra CLI info/debug/verbose output (including diagnostics assigned these levels
through overrides) and resolver tracing remain separate logging parity gaps.
Legacy `use asm` directives are removed, as upstream does, without ending the
directive prologue or suppressing a subsequent `use strict` directive.

## Go API test corpus

`api.json` captures all 33 helper calls in the pinned `pkg/api/api_test.go` and
`api_impl_test.go`: 14 message-formatting cases and 19 directory-prefix cases.
The generator runs their original Go assertions in an isolated package copy,
preserves original call-site lines, and rejects unknown test groups/helpers.
The Rust harness maps all fields explicitly and compares the original expected
output exactly. Capturing the serving layer's prefix helper does not mean the
serving API or JavaScript API test suite has been implemented.

```sh
node scripts/generate_upstream_api_tests.mjs /path/to/pinned/esbuild tests/upstream/api.json
cargo test --lib matches_pinned_upstream_go_api_corpus
```

## Fixture regeneration

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

`js_parser_active.json` contains the zero-based indices of the 7,055 exact-matching
cases enforced in normal `cargo test`. The ignored audit test runs all 8,643 cases
and reports mismatches without treating a completed audit as a conformance pass:

```sh
ESBUILD_RS_UPSTREAM_PARSER_REPORT=/tmp/parser-audit.json cargo test --lib \
  audits_pinned_upstream_js_parser_corpus -- --ignored --nocapture
```

Use `ESBUILD_RS_UPSTREAM_PARSER_INDEX` to select an individual fixture or
`ESBUILD_RS_UPSTREAM_PARSER_TEST` to select an upstream test group (including its
inactive cases) with `matches_pinned_upstream_active_js_parser_corpus`.

The `TestJSX` and `TestJSXAutomatic` groups are fully active. The JSX checkpoint
added 97 matching cases by porting missing diagnostics/suggestions, generated
import ordering, UTF-16 development source positions, and upstream's test-only
`OmitJSXRuntimeForTests` behavior. That option accounts for most automatic-JSX
gains; these are not 97 independent user-facing feature fixes.

The following TSX checkpoint added 33 matching cases: generic component type
arguments return to JSX lexing for attributes, generic async calls are checked
for arrow bodies, and upstream's TSX arrow lookahead distinguishes type
parameters from JSX. Ambiguous `<T>` remains rejected for TSX arrows while the
same spelling is allowed with the TS loader; a comma, default, or constraint can
disambiguate TSX type parameters. More complex type grammar remains in the backlog.

All 231 captured `TestStrictMode` cases are active. Strictness is tracked from
directive parsing and reported with upstream's module/class/directive/JSX reason
notes. Coverage includes duplicate function and parameter declarations,
for-in initializers, contextual label names, and octal property keys. Local
regressions additionally verify note locations and avoid duplicate diagnostics
when minification folds a computed property key.

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

The active bundler tranche exact-compares 836 Unix snapshots and 81
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
when selecting an inactive case. `bundler_additional_active.json` enables 92
reviewed cases beyond the original option-based selection. So 12,297 concrete
upstream cases are currently active in `cargo test`; the remaining 153 captured bundler cases are the parity backlog,
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
