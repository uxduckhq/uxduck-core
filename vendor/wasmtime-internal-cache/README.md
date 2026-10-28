# Wasmtime cache dependency patch

This is the published `wasmtime-internal-cache` 49.0.4 crate from
[Wasmtime](https://github.com/bytecodealliance/wasmtime/tree/2c2970205fe05d6bbf15b9bff6f1e67874b2db5c/crates/cache).
The upstream Apache-2.0 WITH LLVM-exception license is retained in `LICENSE`.
`Cargo.toml.orig` records the original source manifest.

The only change to the published crate is the normalized `Cargo.toml` dependency:
`zstd = "0.13.0"` becomes `zstd = "0.14.3"`, still with default features disabled.
All Rust source, upstream tests and build script are unchanged. Cargo uses this
patch through the root `[patch.crates-io]` entry; the lockfile resolves zstd 0.14.3
and zstd-safe 8.2.0 for the entire workspace.

Extism 1.41.0 requires Wasmtime 49. Wasmtime cache 49.0.4 and 50.0.0 both still
require zstd 0.13, so upgrading Wasmtime alone does not remove these advisories:
[RUSTSEC-2026-0391](https://rustsec.org/advisories/RUSTSEC-2026-0391),
[RUSTSEC-2026-0398](https://rustsec.org/advisories/RUSTSEC-2026-0398), and
[RUSTSEC-2026-0403](https://rustsec.org/advisories/RUSTSEC-2026-0403).

Remove the patch and directory once the Wasmtime version supported by Extism
uses patched zstd releases. Preserve the plugin cache and existing advisory
checks; no advisory is ignored by this patch.
