# S23: terrain download size limit (2026-10-23)

Status: in review. Follow-up to uxduckhq/uxduck-core#3.

## Questions

None. An HTTP response without Content-Length can exceed the intended 16 MiB cap.
The fix bounds bytes accumulated from the response, before terrain validation.
The security impact requires control or influence over the terrain response;
the normal source is HTTPS at terrain.uxduck.com.

## Verification

Host: macOS, arm64; Rust 1.98.1. Used an isolated checkout of PR head
60b10721f815b75c71c49c5ac51df20a592a3926, with its original implementation
temporarily restored for the regression check. All HTTP fixtures use loopback
and dynamically allocated ports.

- Original implementation: 11 focused tests passed, two failed. It accepted a
  completed chunked body of 16 MiB plus one byte, and did not reject that body
  before EOF when the server kept the response open.
- Fixed implementation: all 13 focused tests passed, including both regressions,
  small and exactly-at-limit files with/without Content-Length, an oversized
  declared length with a stalled body, and a failed response stream.
- Command: `cargo test --locked -p uxduck --no-default-features --bin uxduck terrain::tests --jobs 2 -- --test-threads=1`.
- `cargo fmt --all --check` and `git diff --check` passed.
- Focused Clippy passed: `cargo clippy --locked -p uxduck --no-default-features --bin uxduck --tests --jobs 2 -- -D warnings -A unused-imports`.
  The unused-imports allowance covers existing GamepadAxis/GamepadButton imports
  in daemon/controllers.rs in headless builds. All other warnings remain errors.
  The original patch's narrowing cast was removed by keeping the byte limit as
  usize and widening it only for the Content-Length comparison.
- Remote CI and full release verification are pending.
- No UI change; browser interaction does not exercise this native HTTP boundary.

## Maintainer steps

- Carry the fix and inline tests into the source repository before the next Core
  export, so the export preserves them.

## To test

- CI: the updated PR must pass the Linux, macOS and Windows matrix. Full release
  verification remains pending until the required checks pass on its release commit.

## Known gaps

No local Windows/Linux execution, default-feature plugin build, or whole-workspace
suite. The fixtures demonstrate the size-cap bypass, not an actual out-of-memory
crash or compromise of the production terrain service.
