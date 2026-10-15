# Simulation profiles

`ardupilot.json` is the single source for UXDuck's simulation-only parameter
defaults. The native daemon embeds it with `include_str!`; the browser worker
bundles that same file. A missing file fails either build. Native profile-write
errors prevent simulator launch, and browser profile errors surface as a failed
simulation start.

Profiles load after ArduPilot's frame defaults. They change defaults, not saved
parameter values: EEPROM settings survive a restart, and restored snapshots apply
their parameters afterward. Only newly created simulator links use these files;
connecting a vehicle does not apply them. PX4 retains its own airframe defaults.

| Frames                                          | Defaults                                                        | Purpose                                                                                                                                   |
| ----------------------------------------------- | --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `rover`, `motorboat`                            | `MANUAL_STR_EXPO=0.35`, `MANUAL_OPTIONS=1`, `ACRO_TURN_RATE=60` | Softer manual steering around centre, reduced steering above the firmware's speed-scaling base, and a 60°/s requested-turn limit in ACRO. |
| `rover-skid`, `motorboat-skid`                  | `MANUAL_STR_EXPO=0.35`, `ACRO_TURN_RATE=60`                     | Softer steering and the ACRO turn limit, retaining the skid-steering mixer and manual speed-scaling options.                              |
| Other frames, Copter, Plane/VTOL, Sub and Blimp | Upstream defaults                                               | No handling overlay until a profile has been measured against that frame.                                                                 |

Existing modes, throttle response and propulsion settings remain the frame's.
`ACRO_TURN_RATE` is a request limit in ACRO, not a physical turn-rate limit in
MANUAL. These parameters are present in Rover 4.5.7 and the pinned 4.8.0 SITL
build; the firmware owns their implementation.

Validate a profile with:

```sh
cargo test -p uxduck --bin uxduck sim::profiles
cargo test -p uxduck --test daemon a_rovers_default_profile -- --nocapture
pnpm --filter @uxduck/web test src/features/simulation/profiles.test.ts
pnpm --filter @uxduck/web e2e --config playwright.simulation-profile.config.ts
```

The native regression needs `scripts/sitl.sh rover`'s build and compares actual
steering outputs with the old settings at the same stick position. It also checks
low-speed full lock in both directions, neutral stopping, reverse and an EEPROM
override after restart. The Chrome harness needs `scripts/simulator.sh rover`'s
JavaScript/WASM pair; it runs the actual worker and reads the applied settings and
an upstream frame parameter over MAVLink. Both report missing simulator assets
as errors. The Chrome harness uses Vite directly and does not require a built app
or an installed daemon.
