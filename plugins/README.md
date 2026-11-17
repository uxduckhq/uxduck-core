# Writing UXDuck plugins

Start with the [authoring guide](../docs/site/write-a-plugin.md). TypeScript plugins
run as source: no per-plugin build or package installation. The native UXDuck
daemon includes the script host. Browser-only UXDuck does not run plugins.

| Example                                  | Try it for                                       | Inputs                        | Output                                      |
| ---------------------------------------- | ------------------------------------------------ | ----------------------------- | ------------------------------------------- |
| [battery-watch-ts](battery-watch-ts)     | A value, a warning and a tab following a vehicle | Battery telemetry             | Gauge; one warning below 20%, reset at 25%  |
| [fleet-markers-ts](fleet-markers-ts)     | Decoded MAVLink and several vehicles             | GLOBAL_POSITION_INT           | A separate GeoJSON point per vehicle        |
| [session-timer-ts](session-timer-ts)     | A tool independent of a vehicle                  | Startup, wake-up, Reset       | Large readout; state kept across restarts   |
| [home-distance-ts](home-distance-ts)     | A calculation, messages and parameters           | Positions, battery, Mark      | Distance, circle, warning                   |
| [clock-ts](clock-ts)                     | Several panels and persistent timers             | Startup, commands, wake-up    | Clock, stopwatch, timer, alarm              |
| [wind-check-ts](wind-check-ts)           | A preflight check that can hold the launch       | Wind estimate, set by hand    | "Wind under 8 m/s" in each vehicle's checks |
| [adsb-lol-ts](adsb-lol-ts)               | A traffic source from a web service              | Vehicle positions, a timer    | Aircraft from adsb.lol around the vehicles  |
| [traffic-example-ts](traffic-example-ts) | A traffic source on this computer                | A readsb `aircraft.json`      | Aircraft every 5 seconds                    |
| [waypoint-photos](waypoint-photos)       | Rust/WebAssembly and an automation that commands | Waypoints reached, Test photo | A photo at each waypoint, and its answer    |
| [home-distance](home-distance)           | The Rust SDK and richer widgets                  | Positions, battery, commands  | Distance, chart, table, map                 |

Create a deterministic recording without a connected vehicle:

```sh
uxduck plugin new example-fixture --typescript
uxduck plugin test example-fixture
uxduck plugin test plugins/battery-watch-ts --tlog example-fixture/test.tlog
uxduck plugin test plugins/fleet-markers-ts --tlog example-fixture/test.tlog
uxduck plugin test plugins/session-timer-ts --tlog example-fixture/test.tlog --command reset
```

The generated flight lasts 29 seconds and starts with a nearly full battery; it
does **not** test the low-battery warning, multi-vehicle separation, persisted
restart state or a minute-long timer wake-up. Native regression tests cover
those cases separately. Replay prints effects and never sends vehicle commands.
Examples without test.json explicitly report that no expectations were checked.

For Rust examples, install the target with `rustup target add wasm32-unknown-unknown`,
then build them in this folder:

```sh
cd plugins
cargo build --locked --release --target wasm32-unknown-unknown
```

A plugin installs from a folder holding its `plugin.json` and the module it names,
so copy the two together, for example for `home-distance`:

```sh
mkdir -p dist/home-distance
cp home-distance/plugin.json target/wasm32-unknown-unknown/release/home_distance.wasm dist/home-distance/
uxduck plugin install dist/home-distance
```

In UXDuck's main repository, `cargo xtask plugins` does both for every plugin.
