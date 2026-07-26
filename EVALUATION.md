# Experimental closeout evaluation

This is a bounded functional evaluation of an experimental, unaudited,
AI-generated Rust port. It is not an upstream conformance suite, a security
audit, or a production-readiness claim.

## Snapshot

- Rust feature checkpoint: `c463b2d`
- Upstream esbuild: `6ff1d8b0d8c134e867a397eef39702a223ebef9e`
  (version 0.28.1)
- Evaluation date: 2026-07-27
- Toolchain: Rust 1.96.0, Node.js 24.15.0
- Build: `cargo build --release --locked`
- Repository tests: 790 passing
- Approximate behavioral/API parity: 97.0%
- Approximate structural source coverage: 98.7%

The percentages are engineering estimates based on translated subsystems,
translated tests, differential probes, and known gaps. They are not calculated
from an official upstream coverage metric.

## Result

The release-mode Rust CLI passed 9 of 9 representative scenarios. The pinned
upstream executable also passed all scenarios, and the executed bundles had
the same standard output in all 9 cases.

| Scenario | Rust | Upstream | Runtime parity | Rust output | Upstream output |
| --- | --- | --- | --- | ---: | ---: |
| ESM graph and tree shaking | pass | pass | yes | 81 B | 79 B |
| CommonJS `require` | pass | pass | yes | 489 B | 480 B |
| TypeScript modules | pass | pass | yes | 126 B | 122 B |
| JSON loader | pass | pass | yes | 133 B | 133 B |
| JSX transform | pass | pass | yes | 188 B | 184 B |
| Node built-in external | pass | pass | yes | 116 B | 121 B |
| CSS extraction | pass | pass | yes | 92 B | 92 B |
| Source map and metafile | pass | pass | yes | 1,075 B | 1,081 B |
| ESM code splitting | pass | pass | yes | 233 B | 233 B |

Output sizes are the total bytes emitted for each scenario, so the source-map
row includes its map and metafile. Textual output is not expected to be
byte-for-byte identical to upstream; this matrix checks build success,
required artifacts, executable behavior, and selected feature invariants.

## Reproduce

Build the Rust executable and run the self-contained matrix:

```sh
cargo build --release --locked
node scripts/evaluate_bundler.mjs
```

To compare it with an upstream executable:

```sh
UPSTREAM_ESBUILD=/absolute/path/to/esbuild \
  node scripts/evaluate_bundler.mjs
```

`RUST_ESBUILD` can override the Rust executable path. The evaluator creates
temporary fixture projects, bundles them with each executable, runs the
resulting JavaScript with Node.js, checks the generated CSS, source map,
metafile, and split chunk, prints the matrix, and removes the temporary files.

## What remains incomplete

The ordinary one-shot CLI bundling path is useful for experimentation, but the
port still has known parity risk in the API/CLI boundary, bundler, CSS
parser/printer, JavaScript parser/printer, linker, and resolver. The unported
or unverified surface also includes:

- long-running context, rebuild, watch, and serve workflows;
- a compatible plugin host and the JavaScript/Go service protocols;
- WebAssembly and published npm package integration;
- exhaustive package-manager and resolver edge cases;
- exhaustive syntax lowering, diagnostic text, source-map, minifier, and
  cross-platform filesystem parity;
- the full upstream test corpus and performance benchmarking.

The 787 local tests and this 9-case matrix make the snapshot inspectable and
useful, but upstream esbuild should remain the production choice.
