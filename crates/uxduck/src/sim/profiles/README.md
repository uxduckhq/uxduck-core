# Simulation profiles

`ardupilot.json` is the single source for UXDuck's simulation-only parameter
defaults, and for the physics of a frame UXDuck flies differently (`physics`:
ArduPlane's aerodynamic coefficients, written beside the profile and run as
`--model <frame>:uxduck-physics.json`; coefficients not given keep the frame's). The native daemon embeds it with `include_str!`; the browser worker
bundles that same file. A missing file fails either build. Native profile-write
errors prevent simulator launch, and browser profile errors surface as a failed
simulation start.

Profiles load after ArduPilot's frame defaults. They change defaults, not saved
parameter values: EEPROM settings survive a restart, and restored snapshots apply
their parameters afterward. Only newly created simulator links use these files;
connecting a vehicle does not apply them. PX4 retains its own airframe defaults.

| Frames                                              | Defaults                                                                                          | Purpose                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rover`, `motorboat`                                | `MANUAL_STR_EXPO=0.35`, `MANUAL_OPTIONS=1`, `ACRO_TURN_RATE=60`                                   | Softer manual steering around centre, reduced steering above the firmware's speed-scaling base, and a 60°/s requested-turn limit in ACRO.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `rover-skid`, `motorboat-skid`                      | `MANUAL_STR_EXPO=0.35`, `ACRO_TURN_RATE=60`                                                       | Softer steering and the ACRO turn limit, retaining the skid-steering mixer and manual speed-scaling options.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `plane-jet` (the Jet, S143)                         | Cruise 200 m/s, maximum 220 (790 km/h), minimum 25; `c_drag_p` 0.0236; retuned roll and TECS      | Flies like the Reaper jet drone (790 km/h with a 1.2 kg payload). Starts from ArduPlane's `plane-jet.parm` and its 22 kg plane physics with only the parasitic drag lowered (upstream 0.1, about 100 m/s), so take-off and low-speed handling stay upstream's. A 4 km circle (46° of bank at 200 m/s), 1 km waypoint radius, a 6 km take-off leg, TECS holding height by pitch alone (`TECS_SPDWEIGHT` 0, so it never dives for speed after the in-air hand-over), 100 m/s climb and 30 m/s sink limits with ±45° pitch, ±70° of roll (at 200 m/s a 55° bank turns no tighter than 2.9 km, too close to the circle for navigation to capture it: it swung 3.4 km outside before settling), the earlier roll gains with damping, ArduPlane's default throttle slew rate and no runway roll. `BATT_CAPACITY` 45 Ah keeps the battery gauge near an hour and a quarter at cruise's 35 A (SITL's own battery is unlimited). |
| `blimp-motor`                                       | LOITER forward 1 m/s, lateral 0.7 m/s, vertical 0.5 m/s                                           | Faster small-motor practice controls without changing the original toy physics. Throttle controls height, pitch commands forward/reverse, roll commands lateral movement, yaw turns.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `blimp-motor-inspection-v1`                         | LOITER forward 8 m/s, lateral 1.5 m/s, vertical 0.75 m/s, yaw 0.2 rad/s; matched controller gains | Generic 10 m neutral-buoyancy training model with versioned mass, inertia, motor and drag coefficients. Requires the matching SITL extension and embedded physics JSON; not measured HyLight performance.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Other frames, Copter, Plane/VTOL, Sub and fin Blimp | Upstream defaults                                                                                 | No handling overlay until a profile has been measured against that frame.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |

`camera` and `gimbal`, at the top of the file, are added only when a simulation
carries a simulated camera or gimbal (ADR-0063 Y3, S403): `CAM1_TYPE=6` and
`MNT1_TYPE=6` make the autopilot's first camera and first mount MAVLink ones,
which the daemon's simulated payload plays. A program's own `gimbal` is added
with them: `SCHED_LOOP_RATE=100` for `arduplane` and `ardurover`, which at their
default of 50 find the gimbal and never send it a target (S400). None of them is
written without the payload, because a vehicle told of a gimbal that is not
there refuses to arm. The browser worker reads none of them, because UXDuck Web
simulates neither.

The kind of camera and gimbal a simulation carries (ADR-0063 Y16, S410) decides
which of the two the autopilot is told of. A camera that only sends video
(`stream-only`) has nothing for an autopilot to trigger or set, so `camera` is
left out and `CAM1_TYPE` stays 0. A camera and gimbal in one (`integrated`)
manages its own gimbal on the camera's component, so `gimbal` is left out and
`MNT1_TYPE` stays 0: told of a mount, the autopilot would wait for a gimbal
device that no component is, and refuse to arm. A camera that takes photos in
one mode and records in the other (`photo-video`) is told of as the usual one
is. A thermal and tracking camera (`thermal-tracking`, S412) is a camera and gimbal
in one with a second sensor and tracking, so the autopilot is told the same as
for `integrated`. Real ArduCopter flew with each on 2026-11-15.

`ardupilotGimbals` names the gimbals ArduPilot simulates itself, which a simulation
carries in place of the simulated camera and gimbal (ADR-0063 Y4, S409). Each has
its `device`, ArduPilot's name for the simulated device, which the daemon puts
on the autopilot's serial port 2 as `--serial2=sim:<device>` (serial 1 is
simulated traffic's), and the `parameters` that tell the autopilot of it: its
mount's type, its camera's type and that port's protocol. `viewpro` is
`MNT1_TYPE=11`, `CAM1_TYPE=4` and `SERIAL2_PROTOCOL=8`. `avt-cm62` is
`MNT1_TYPE=6`, `CAM1_TYPE=6` and `SERIAL2_PROTOCOL=2`. The CM62 is a MAVLink
gimbal (`mavlink`), so a program's own `gimbal` goes with it: the faster loop
on `arduplane` and `ardurover`. Viewpro follows at their default loop (run on
both, 2026-11-15). The daemon reads a build's file for each `device` name to
know which it can simulate, and refuses a start with one it cannot. The browser
worker reads none of them, because UXDuck Web runs none.

`ardupilotGimbalsNotOn` names the models of ArduPilot's simulator that run none
of these gimbals, as `--model` names them: `motorboat` and `sailboat`,
`balancebot`, `singlecopter` and `coaxcopter`, and `calibration`. Their physics
never runs a simulated device on a serial port (their classes do not call
`update_external_payload`, ArduPilot `d5420ff97c84`), so the autopilot is told
of a mount it cannot find, says "PreArm: Mount: not healthy" and does not arm.
Each was started with Viewpro on 2026-11-15 and none found it. A frame is one of
them when its name starts with the model's, as ArduPilot matches a model. The
daemon refuses a start with one of ArduPilot's own gimbals on such a frame, and
the engine refuses it for every boat. Add a model here when ArduPilot gains one
whose `update` leaves that call out.

Existing modes, throttle response and propulsion settings remain the frame's.
`ACRO_TURN_RATE` is a request limit in ACRO, not a physical turn-rate limit in
MANUAL. These parameters are present in Rover 4.5.7 and the pinned 4.8.0 SITL
build; the firmware owns their implementation.

Validate a profile with:

```sh
cargo test -p uxduck --bin uxduck sim::profiles
cargo test -p uxduck --test daemon a_rovers_default_profile -- --nocapture
cargo test -p uxduck --test daemon a_simulated_multicopter_carries -- --nocapture
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
