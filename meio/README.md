# meio

[![Crates.io][crates-badge]][crates-url]
[![Released API docs][docs-badge]][docs-url]

[crates-badge]: https://img.shields.io/crates/v/meio.svg
[crates-url]: https://crates.io/crates/meio
[docs-badge]: https://docs.rs/meio/badge.svg
[docs-url]: https://docs.rs/meio

Lightweight async actor framework for Rust.

## Usage

Check tests in `lib.rs` file to see how it works.

## WASM

It has experimental WASM support. To activate use:

```toml
meio = { default-features = false, features = ["wasm"] }
```
