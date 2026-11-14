/**
 * Write UXDuck plugins in TypeScript (ADR-0006 X6, X7). A plugin is one
 * `.ts` file and a `plugin.json`; UXDuck strips the types and runs it in the
 * same sandbox, with the same limits, as any plugin. There are three ways
 * to package a plugin, in this order:
 *
 * 1. **TypeScript**: this package. No build step.
 * 2. **Compiled WebAssembly**: Rust (`crates/uxduck-sdk`), Go, Zig or C
 *    against the same contract, for heavy work.
 * 3. **Process plugins**: a native binary, for what needs the operating
 *    system; not sandboxed (later).
 *
 * A script exports any of `describe`, `onItems`, `onMessages`, `onFrames`,
 * `onCommand`, `onTick`, `onStart` and `onFetched`, each taking the input
 * below and returning `Effects`. `Effects.fetch` asks for a page from a host
 * the manifest's `hosts` names, over HTTPS; the answer comes to `onFetched`.
 * `Effects.traffic` reports aircraft, which join the Airspace tab while the
 * plugin is on. `Effects.checks` says where the preflight checks the
 * manifest's `checks` declare stand for each vehicle (`check` below); a
 * `block` check holds the launch only once a person allows it. `onTick` runs every `subscribe.tickMs` milliseconds,
 * whatever arrives, for a clock or a timer. `onStart` runs once, after
 * `describe`, with what the plugin kept: `Effects.store` keeps keys across
 * restarts of the plugin and of UXDuck, `null` removing one, 64 KB in all.
 * Imports from "@uxduck/sdk" are the helpers here; nothing else can be
 * imported.
 */

export type {
  CommandCall,
  DeclaredCheck,
  Description,
  Effects,
  FetchRequest,
  FetchedInput,
  Frame,
  FrameBatch,
  ItemBatch,
  ItemInput,
  MapFeature,
  MessageBatch,
  MessageInput,
  Panel,
  PluginManifest,
  PluginNotification,
  PluginTraffic,
  PluginTrafficStatus,
  ProducedItem,
  ReportedCheck,
  ReportedStatus,
  SendCommand,
  SendMessage,
  Severity,
  StartInput,
  TickInput,
  Tone,
  Unit,
  Widget,
} from "@uxduck/protocol";
export {
  check,
  circle,
  commandLong,
  latest,
  notify,
  point,
  table,
  text,
  value,
} from "./runtime.js";
