---
title: Write a plugin
order: 11
group: Preview features
summary: Develop and test a plugin; the Plugins page is hidden in production.
availability: Preview guide. The Plugins page and in-app AI chat are currently hidden in production builds. These instructions apply to development and staging builds.
---

Plugins add your own values, panels, map features and commands to UXDuck.

## Three ways to write one

1. **TypeScript**: one `.ts` file and a `plugin.json`, with no build step. Start here.
2. **Compiled WebAssembly**: Rust (`crates/uxduck-sdk`), Go, Zig or C against the same contract, for heavy work.
3. **Process plugins**: a native program, for what needs the operating system. Not yet available.

## Start one

```
uxduck plugin new flight-time --typescript
uxduck plugin test flight-time
```

The first writes `plugin.json`, `index.ts` and a test: `test.tlog`, a short recorded flight, and `test.json`, the items it must produce. The second runs it against the flight, prints what it was given and what it returned, and checks the test. Install it with `uxduck plugin install flight-time`, or with **Install from a folder** on the **Plugins** page.

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

## Keeping state

`store` keeps keys across restarts of the plugin and of UXDuck: `{ store: { timer: { endsAt } } }` sets one, `null` removes one. A plugin keeps at most 64 KB, on this computer only, and gets it back in `onStart`.

## Your vendor's messages

A plugin can bring its vendor's MAVLink messages as a dialect file: MAVLink's XML, beside `plugin.json`, named in `"dialect": "acme.xml"`. While the plugin runs, its messages are decoded for it and sent by name like the ArduPilot dialect's; list them in `subscribe.messages` and `sends` as usual. Only `<messages>` is read, so `<include>` and enums may stay. A message whose id or name the ArduPilot dialect or another plugin already uses is refused at install.

`uxduck dev decode flight.tlog --plugin <folder>` prints a tlog's messages by name with that dialect.

## Commands

Each command in `plugin.json` has a `name`, a `title`, a `description`, its `parameters` as JSON Schema and a `risk` (`none`, `confirm` or `dangerous`). A `none` command may suggest a `shortcut`, such as `Alt+C`; it is left unbound when it clashes with one of UXDuck's own. Commands appear in the input (type `>` and their title) and the agent runs them after an operator allows it.

## Ask the agent to write one

Ask in a chat for a value or a panel UXDuck does not have, such as "show the distance from home". The agent drafts a TypeScript plugin, tests it against the vehicle's last five minutes and says what it produced. Say "install it" when you want it: you are always asked to approve an install, with what the plugin may do and the chat it came from. Drafts are listed on the **Plugins** page with their chat.

## On your other computers, and shared

With sync on, a plugin you install follows you to your other computers. There it reads only: anything more it asks for, such as commanding vehicles, waits for you on that computer's **Plugins** page until you choose **Allow**. Each new version waits again. Removing it where you installed it removes it everywhere.

**Share link**, under a plugin's **What it may do and uses** on the **Plugins** page, makes a link anyone signed in to UXDuck can install from. They see who shared it and what it asks for, and decide themselves. Shared plugins are not reviewed by UXDuck. **Stop sharing** turns the link off; copies already installed stay.

## Panels and the map

Panels in the Control tab are built from `text`, `value`, `gauge`, `chart`, `table`, `display`, `button` and `time-input` widgets. Each may have `help`, shown behind an info button, and a `muted` or `warning` tone. A table's rows are an item's text, as the SDK's `table` writes them. A `display` is one item, large; a `button` may carry `params` for its command. A `time-input` is a time of day with **Set**: its command gets `{ "at": … }`, the next time the person's clock reads it, in seconds since 1970.

A panel with `"tab": true` is a tab of its own instead. A plugin's tab panels share one tab, with a switch between them, opened from **+** › **Plugins** under the plugin's name. It follows no vehicle.

Map features are GeoJSON geometries with an id; sending the same id again moves it. They are drawn in the plugin's own layer, which **Layers** above the map hides.

## Limits

Each plugin gets 16 MiB of memory and one second per call. A plugin that runs out of either is stopped, and UXDuck says why. The **Plugins** page shows what each one is using, and turns it off without removing it.
