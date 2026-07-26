# Final experimental porting status

Active feature porting stopped after commit `c463b2d`. The final engineering
estimate is 97.0% behavioral/API parity and 98.7% structural source coverage
against the pinned upstream revision. These figures are approximate, not an
official coverage metric. See [EVALUATION.md](EVALUATION.md) for the release
binary evaluation and the important unverified surfaces.

Status values:

- `not started`: no Rust implementation
- `in progress`: some source files translated, parity incomplete
- `ported`: source translated and focused parity tests pass
- `verified`: package-level and integration parity tests pass

| Upstream package | Status |
| --- | --- |
| `api` | in progress |
| `cmd/esbuild` | in progress |
| `internal/api_helpers` | ported |
| `internal/ast` | ported |
| `internal/bundler` | in progress |
| `internal/cache` | ported |
| `internal/cli_helpers` | ported |
| `internal/compat` | ported |
| `internal/config` | ported |
| `internal/css_ast` | ported |
| `internal/css_lexer` | ported |
| `internal/css_parser` | in progress |
| `internal/css_printer` | in progress |
| `internal/fs` | ported |
| `internal/graph` | ported |
| `internal/helpers` | ported |
| `internal/js_ast` | ported |
| `internal/js_lexer` | ported |
| `internal/js_parser` | in progress |
| `internal/js_printer` | in progress |
| `internal/linker` | in progress |
| `internal/logger` | ported |
| `internal/renamer` | ported |
| `internal/resolver` | in progress |
| `internal/runtime` | ported |
| `internal/sourcemap` | ported |
| `internal/xxhash` | ported |
