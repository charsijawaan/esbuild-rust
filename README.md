# esbuild-rs

This repository is a source-faithful Rust port of
[evanw/esbuild](https://github.com/evanw/esbuild).

The port follows esbuild's package and source-file boundaries wherever Rust's
type and ownership systems allow it. Behavioral compatibility with the pinned
upstream revision is the acceptance criterion. Upstream tests are translated
or exercised as differential tests as each subsystem lands.

This is an in-progress port and is not yet usable as an esbuild replacement.
See [PORTING.md](PORTING.md) for the live package matrix and [UPSTREAM.md](UPSTREAM.md)
for the exact upstream revision.

## Development

```sh
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

The project retains esbuild's MIT license.

