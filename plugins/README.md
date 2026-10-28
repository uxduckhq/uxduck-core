# Writing UXDuck plugins

Start with the [authoring guide](../docs/site/write-a-plugin.md). TypeScript plugins
run as source: no per-plugin build or package installation. The native UXDuck
daemon includes the script host. Browser-only UXDuck does not run plugins.

| Example                              | Try it for                                       | Inputs                        | Output                                      |
| ------------------------------------ | ------------------------------------------------ | ----------------------------- | ------------------------------------------- |
| [battery-watch-ts](battery-watch-ts) | A value, a warning and a tab following a vehicle | Battery telemetry             | Gauge; one warning below 20%, reset at 25%  |
| [fleet-markers-ts](fleet-markers-ts) | Decoded MAVLink and several vehicles             | GLOBAL_POSITION_INT           | A separate GeoJSON point per vehicle        |
| [session-timer-ts](session-timer-ts) | A tool independent of a vehicle                  | Startup, wake-up, Reset       | Large readout; state kept across restarts   |
| [home-distance-ts](home-distance-ts) | A calculation, messages and parameters           | Positions, battery, Mark      | Distance, circle, warning                   |
| [clock-ts](clock-ts)                 | Several panels and persistent timers             | Startup, commands, wake-up    | Clock, stopwatch, timer, alarm              |
| [wind-check-ts](wind-check-ts)       | A preflight check that can stop arming           | Wind estimate, set by hand    | "Wind under 8 m/s" in each vehicle's checks |
| [camera-manager](camera-manager)     | Rust/WebAssembly and commanding hardware         | Camera acknowledgement, Photo | Camera command and count                    |
| [home-distance](home-distance)       | The Rust SDK and richer widgets                  | Positions, battery, commands  | Distance, chart, table, map                 |

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

For Rust examples, install the target with `rustup target add wasm32-unknown-unknown`
and run `cargo xtask plugins` at the repository root. Built folders go into
`plugins/dist/<manifest-id>`. Install from there.
