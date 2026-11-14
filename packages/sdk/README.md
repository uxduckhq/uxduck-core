# @uxduck/sdk

Types and helpers for UXDuck's TypeScript plugin sandbox.

- [Write a plugin](../../docs/site/write-a-plugin.md): scaffold, manifest, handlers,
  units, replay, installation, panels and troubleshooting.
- [Runnable examples](../../plugins/README.md): values, warnings, fleet maps,
  persistent timers, commands and Rust equivalents.
- [Exported types](src/index.ts) and [helper signatures](src/runtime.d.ts).

This package is not published for `npm install`. UXDuck resolves `@uxduck/sdk`
inside the sandbox itself. For editors, [plugins/tsconfig.json](../../plugins/tsconfig.json)
maps `@uxduck/sdk` to `src/index.ts`, and the wire types it uses to
`packages/protocol/src/generated/types.gen.ts`, generated from the contracts. For a
plugin outside `plugins/`, copy those `paths`, pointing them at your checkout. This
affects editing, not runtime loading.

Check every TypeScript example with `npx -p typescript tsc -p plugins` from the
repository root. Handlers return effects synchronously, and only SDK imports are
available at runtime.
