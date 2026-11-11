# MAVInspect dependency patch

This is the published `mavinspect` 0.6.6 crate from
[MAVInspect](https://gitlab.com/mavka/libs/mavinspect/-/tree/14aea1d018f01809b84a84d65b564ea824b92744).
The upstream MIT and Apache-2.0 licenses are retained in `LICENSE-MIT` and
`LICENSE-APACHE`. `Cargo.toml.orig` records the original source manifest.

mavio's dialect code generation reads MAVLink XML through MAVInspect, which
depends on `serde_yml` and its `libyml` backend. Both are unsound and
unmaintained (GHSA-gfxp-f68g-8x78, GHSA-hhw4-xg65-fp2x), with no fixed release.
MAVInspect 0.7.0-rc.1 still depends on them.

The changes to the published crate:

- `serde_yml = "0.0.12"` becomes `serde-saphyr = "2.0.0"`, and the one call
  that reads `.dialects-metadata.yml` (`src/parser/inspector.rs`) uses
  `serde_saphyr::from_str`.
- `build.rs` is removed. It ran `git submodule update --init` in the crate's
  directory, which inside this repository would act on UXDuck's checkout.
- The bundled MAVLink XML, tests, examples and their manifest entries are
  removed; they were used only by MAVInspect's own tests.

All other Rust source is unchanged. Cargo uses this patch through the
`[patch.crates-io]` entries in the root and `plugins/` manifests.

Remove the patch and directory once a MAVInspect release used by mavio no
longer depends on `serde_yml`.
