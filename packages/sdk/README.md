# @uxduck/sdk

Types and helpers for UXDuck's TypeScript plugin sandbox.

- [Write a plugin](../../docs/site/write-a-plugin.md): scaffold, manifest, handlers,
  units, replay, installation, panels and troubleshooting.
- [Runnable examples](../../plugins/README.md): values, warnings, fleet maps,
  persistent timers, commands and Rust equivalents.
- [Exported types](src/index.ts) and [helper signatures](src/runtime.d.ts).

This is a private workspace package. It is not currently published for external
`npm install`. UXDuck resolves `@uxduck/sdk` inside the sandbox itself. For editor
types outside this workspace, point TypeScript's `paths["@uxduck/sdk"]` at the
checkout's `packages/sdk/src/index.ts`; this affects editing, not runtime loading.

Run `pnpm --filter @uxduck/sdk typecheck` to check every TypeScript example.
Handlers return effects synchronously; only SDK imports are available at runtime.
