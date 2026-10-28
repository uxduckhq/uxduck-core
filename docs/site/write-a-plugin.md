---
title: Write a plugin
order: 11
group: Preview features
summary: Develop and test a plugin; the Plugins page is hidden in production.
availability: Preview guide. The Plugins page and in-app AI chat are currently hidden in production builds. These instructions apply to development and staging builds.
---

Plugins add your own values, panels, map features and commands to UXDuck.

The **native daemon** runs plugins, including when its UI is open in Chrome.
Browser-only UXDuck has no plugin host. Develop against a local native daemon;
the folder you install must be readable on that daemon's computer.

## Three ways to write one

1. **TypeScript**: one `.ts` file and a `plugin.json`, with no build step. Start here.
2. **Compiled WebAssembly**: Rust (`crates/uxduck-sdk`), Go, Zig or C against the same contract, for heavy work.
3. **Process plugins**: a native program, for what needs the operating system. Not yet available.

## Start one

```
uxduck plugin new battery-watch --typescript
uxduck plugin test battery-watch
```

The first writes `plugin.json`, `index.ts` and a test: `test.tlog`, a short synthetic flight, and `test.json`, the items it must produce. Every scaffold starts as a **battery readout**, regardless of its name. The second runs without a daemon, prints each call's input and effects as JSON lines, and checks the test. A successful run exits 0 and prints `test.json: passed` on stderr.

Install with `uxduck plugin install battery-watch`, or **Install from a folder** on
the **Plugins** page. The daemon must be running. Edit the source, test again, and
install the same folder again to reload it; there is no file watcher.

## A first useful change

The manifest selects inputs; the script returns outputs. Keep both in step:

```json
{
  "abi": 2,
  "id": "battery-watch",
  "name": "Battery watch",
  "version": "0.1.0",
  "script": "index.ts",
  "capabilities": ["read"],
  "subscribe": {
    "items": { "keys": ["battery.remaining_pct"], "cadenceMs": 1000 }
  }
}
```

```ts
import type { Description, Effects, ItemBatch } from "@uxduck/sdk";
import { value } from "@uxduck/sdk";

export function describe(): Description {
  return {
    panels: [
      {
        id: "battery",
        title: "Battery",
        tab: true,
        vehicle: true,
        widgets: [{ type: "value", item: "battery_watch.remaining_pct", label: "Battery" }],
      },
    ],
  };
}

export function onItems(batch: ItemBatch): Effects {
  return {
    items: batch.items.flatMap((item) =>
      item.key !== "battery.remaining_pct" || item.value == null
        ? []
        : [value(item.vehicleId, "battery_watch.remaining_pct", "Battery", "percent", item.value)],
    ),
  };
}
```

Change `test.json` to `{"expect":{"items":["battery_watch.remaining_pct"]}}` when
changing the output key. Open the tab from **+** › **Plugins** › **Battery watch**.
Before using other telemetry keys, inspect `uxduck rpc telemetry/get '{"vehicleId":"…"}'`.
Subscriptions deliver updates rather than a complete snapshot; retain earlier
values per vehicle when a calculation needs several keys across batches.

## Test another idea

The repository's `plugins/README.md` indexes runnable examples: battery warnings,
fleet map markers, a persistent session timer, distance from home, clocks and a
Rust camera controller. Reuse a scaffold's recording with any source folder:

```sh
uxduck plugin test plugins/fleet-markers-ts --tlog battery-watch/test.tlog
uxduck plugin test plugins/session-timer-ts --tlog battery-watch/test.tlog --command reset
uxduck plugin test plugins/home-distance-ts --tlog battery-watch/test.tlog --command mark --params '{"label":"Survey start"}'
```

`test.json` has the exact shape `{"expect":{"items":["group.key"]}}`, with at least one key. Every listed
key must be produced; malformed expectations fail. Without that optional file,
the command reports **replay only**, with no assertions. Key presence does not
assert a numeric value, a warning threshold, a map shape or a command's outcome.
The generated 29-second recording cannot verify a low-battery warning or a
minute-long wake-up. Use a suitable recording and check the actual effects.

Replay validates item naming and cumulative storage, and runs handlers in a
sandbox. It does not send outbound commands, exercise operator approvals, or
prove that every effect will be accepted by the engine. Verify installation,
permissions, rendering and restart behavior against a disposable daemon too.

## What it receives

A script exports any of these, each taking its input and returning effects:

- `onItems`: telemetry items by key or `prefix.*`, already in units.
- `onMessages`: MAVLink messages by name, decoded, with MAVLink's field names and raw units.
- `onFrames`: raw frames by message id, for messages the ArduPilot dialect does not have.
- `onCommand`: one of its commands, with its parameters, and no vehicle when it came from the plugin's own tab.
- `onTick`: every `subscribe.tickMs` milliseconds, whatever arrives, or once when it asked with `wakeInMs`. Prefer the wake-up: a plugin that only counts can sleep until something happens.
- `onStart`: once, after `describe`, with what it kept.
- `describe`: its panels.

Effects are commands, messages by name (only those in `sends`), items with a unit, notifications, map features, log lines and `store`. An item's key ends in its unit, such as `home.distance_m`, `stopwatch.elapsed_s` for seconds or `clock.now_at` for a moment. An item with no vehicle is the plugin's own, shown in its tab; give it `ratePerS` (1 for a running stopwatch) and the app counts on between updates. Helpers come from `@uxduck/sdk`; nothing else can be imported.

Handlers are synchronous. There is no DOM, Node runtime, filesystem, fetch or
package loader. The SDK exposes `value`, `text`, `table`, `latest`, `notify`,
`point`, `circle` and `commandLong`; its TypeScript types live in
`packages/sdk/src/index.ts`. This package is currently private to the repository,
so an external editor needs a checkout and a TypeScript path mapping to that file;
`uxduck plugin test` resolves the import without package installation.

An item key is `group.name`, lower case with underscores. Units require their
suffix: meter `_m`, meter-per-second `_mps`, percent `_pct`, volt `_v`, ampere
`_a`, degree `_deg`, second `_s`, time `_at`. Unitless text and counts have no
unit suffix. `timeUs` is microseconds; `nowMs` and `Date.now()` are milliseconds;
an item's `time` unit is **Unix seconds**. During replay, handler clocks advance
with the recording rather than the time taken to run it.

## Keeping state

`store` keeps keys across restarts of the plugin and of UXDuck: `{ store: { timer: { endsAt } } }` sets one, `null` removes one. A plugin keeps at most 64 KB, on this computer only, and gets it back in `onStart`.

## Your vendor's messages

A plugin can bring its vendor's MAVLink messages as a dialect file: MAVLink's XML, beside `plugin.json`, named in `"dialect": "acme.xml"`. While the plugin runs, its messages are decoded for it and sent by name like the ArduPilot dialect's; list them in `subscribe.messages` and `sends` as usual. Only `<messages>` is read, so `<include>` and enums may stay. A message whose id or name the ArduPilot dialect or another plugin already uses is refused at install.

`uxduck dev decode flight.tlog --plugin <folder>` prints a tlog's messages by name with that dialect.

## Commands

Each command in `plugin.json` has a `name`, a `title`, a `description`, its `parameters` as JSON Schema and a `risk` (`none`, `confirm` or `dangerous`). A `none` command may suggest a `shortcut`, such as `Alt+C`; it is left unbound when it clashes with one of UXDuck's own. Commands appear in the input (type `>` and their title) and the agent runs them after an operator allows it.

## Preflight checks

A plugin can add checks to each vehicle's preflight list, such as the wind from a forecast or a payload's SD card. Declare them in `plugin.json`: `"checks": [{ "id": "wind", "title": "Wind under 8 m/s", "severity": "block", "staleMs": 30000 }]`. Report where each stands for a vehicle in `checks`, with the SDK's `check(vehicleId, "wind", "pass" | "fail" | "waiting", "Under 8 m/s.")`. Keep the words short and without live numbers, and show a value as an item instead.

The latest report counts until `staleMs` runs out (a minute if left out). After that the check is unknown. It is also unknown while the plugin is off or stopped. A `block` check stops arming only after the person allows it, when installing the plugin or on the **Plugins** page. Until then it only warns. Reports of checks you did not declare are ignored. [wind-check-ts](../../plugins/wind-check-ts) is an example.

## Ask the agent to write one

Ask in a chat for a value or a panel UXDuck does not have, such as "show the distance from home". The agent drafts a TypeScript plugin, tests it against the vehicle's last five minutes and says what it produced. Say "install it" when you want it: you are always asked to approve an install, with what the plugin may do and the chat it came from. Drafts are listed on the **Plugins** page with their chat.

## On your other computers, and shared

With sync on, a plugin you install follows you to your other computers. There it reads only: anything more it asks for, such as commanding vehicles, waits for you on that computer's **Plugins** page until you choose **Allow**. Each new version waits again. Removing it where you installed it removes it everywhere.

**Share link**, under a plugin's **What it may do and uses** on the **Plugins** page, makes a link anyone signed in to UXDuck can install from. They see who shared it and what it asks for, and decide themselves. Shared plugins are not reviewed by UXDuck. **Stop sharing** turns the link off; copies already installed stay.

## Panels and the map

Panels in the Cockpit tab are built from `text`, `value`, `gauge`, `chart`, `table`, `display`, `button` and `time-input` widgets. Each may have `help`, shown behind an info button, and a `muted` or `warning` tone. A table's rows are an item's text, as the SDK's `table` writes them. A `display` is one item, large; a `button` may carry `params` for its command. A `time-input` is a time of day with **Set**: its command gets `{ "at": … }`, the next time the person's clock reads it, in seconds since 1970.

A panel with `"tab": true` is a tab of its own instead. A plugin's tab panels share one tab, with a switch between them, opened from **+** › **Plugins** under the plugin's name. It follows no vehicle.

Add `"vehicle": true` to a tab panel to follow the selected vehicle and allow
pinning it. Its widgets can read vehicle telemetry as well as plugin items.
Independent tabs omit this flag and produce items with `value(null, …)`. Their
button handlers receive no `vehicleId`; check it before commanding a vehicle.

Map features are GeoJSON geometries with an id; sending the same id again moves it. They are drawn in the plugin's own layer, which **Layers** above the map hides.

Include the vehicle id in feature ids for a fleet, so one vehicle cannot overwrite
another's marker. GeoJSON coordinates are `[longitude, latitude]`; the SDK's
`point(id, latitude, longitude)` and `circle` helpers take latitude first.

## When nothing appears

| Symptom                        | Check                                                                                         |
| ------------------------------ | --------------------------------------------------------------------------------------------- |
| Missing test.tlog              | Run the scaffold, or pass `--tlog <recording>`; no live vehicle is recorded automatically     |
| No handler calls or values     | Verify keys/message names in the manifest and whether the recording contains those inputs     |
| Value discarded or a dash      | Match the key's unit suffix, widget item key and vehicle; guard absent values                 |
| No plugin tab                  | Add `tab: true`, reinstall, then open it from **+** › **Plugins**                             |
| Map markers replace each other | Give every vehicle a distinct feature id                                                      |
| State disappears on restart    | Return `store` and restore it in onStart; module variables alone are temporary                |
| Import fails                   | Use only `@uxduck/sdk`; local files and npm dependencies are unavailable                      |
| Plugin stopped                 | Read its status detail on Plugins; test again for syntax, memory, execution or storage errors |
| Agent test returns no data     | Inspect error, flightS and calls; zero flightS does not verify a telemetry calculation        |

## Limits

Each plugin gets 16 MiB of memory and a budget of 100 million WebAssembly fuel units per call, including initialization. Fuel measures executed work, so sleeping or pausing the computer does not spend the budget. A plugin that runs out of memory or exhausts its execution budget is stopped, and UXDuck says why. The **Plugins** page shows what each one is using, and turns it off without removing it.
