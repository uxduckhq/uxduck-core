---
title: Your own simulator
search: Connect ArduPilot SITL, PX4 SITL or Gazebo running on this computer or another
order: 4
group: Get started
summary: See a SITL, PX4 or Gazebo simulation you run yourself, on this computer or another, as a simulated vehicle.
---

A simulator that speaks MAVLink is a vehicle as soon as its messages reach UXDuck. Nothing needs installing beside it.

## Where to send

Open **Home › Connect a real vehicle › Your own simulator**. It shows this computer's addresses and the UDP port UXDuck listens on (15010, or the next free one up to 15019). Send the simulator's MAVLink there.

## ArduPilot SITL

```
sim_vehicle.py -v ArduCopter --out=udp:192.168.1.20:15010
```

Use the address shown under **Your own simulator**. On the same computer, `127.0.0.1` works.

## PX4 SITL

PX4 sends to UDP port 14550 on its own computer. Route it on with mavlink-router:

```
mavlink-routerd -e 192.168.1.20:15010 127.0.0.1:14550
```

## MAVProxy

```
mavproxy.py --master=tcp:127.0.0.1:5760 --out=udp:192.168.1.20:15010
```

## Your fork's parameters and messages

A SITL built from your own ArduPilot fork reports the commit it was built from, as hardware does, so the fork's parameter descriptions and MAVLink messages apply to it by themselves. Add them once from the checkout that built it:

```
uxduck params definitions generate path/to/ardupilot --vehicle Copter
uxduck mavlink dialects generate path/to/ardupilot
```

The vehicle's setup shows the commit beside the firmware version. See [Parameters](parameters.md) for choosing them per vehicle.

## Let UXDuck start your own build

UXDuck can also start and stop a simulator built from your own ArduPilot checkout, as it does its downloaded ones.

A simulator only runs on the kind of computer it was built for: its system (macOS or Linux) and its processor (such as Apple silicon or Intel, x86-64 or 64-bit Arm). Build it on the computer that runs UXDuck; a build from another kind of computer may not start.

Use your own ArduPilot checkout, with its submodules and ArduPilot's build tools installed: the `install-prereqs` script for your system in `Tools/environment_install` (see ArduPilot's [Building the code](https://ardupilot.org/dev/docs/building-the-code.html)). Build it with UXDuck, once for each commit, giving your checkout's path:

```
uxduck sim build path/to/your/ardupilot
```

Then choose the path it prints under **New simulation › Advanced › Your own build**. The ⓘ beside that field repeats these steps. **Save to your builds** keeps it, named, in **Settings › Custom firmware › Simulator builds**, where each build says which firmware, version and commit it is, read from the file without running it. **Saved build** then offers it in New simulation.

It builds the checked-out commit in a copy of its own (your files and `build/` folder are left alone; changes not committed are left out), adds UXDuck's local-only network patch so the simulator can't be reached from other computers, and checks that it can't. Use `--vehicle plane` (or `rover`, `submarine`, `airship`) for another vehicle; macOS and Linux only. Instead of choosing it in the form, you can run `uxduck sim start --binary <path>`.

On Windows, `uxduck sim build` isn't available. Compile SITL with Cygwin as ArduPilot's [Windows guide](https://ardupilot.org/dev/docs/building-setup-windows-cygwin.html) describes, keep the DLLs it needs (such as `cygwin1.dll`) beside the `.exe`, and use it as an existing executable, below. It runs without UXDuck's local-only network patch, so it needs **Allow network access for this build**. Or run SITL under WSL and send its MAVLink to UXDuck, as under **Where to send**.

The patch has a version for current ArduPilot and for 4.5. For another release, `uxduck sim build --allow-network` builds it without one: while it runs, other computers on your network can connect to it, send it RC input and receive its state.

## Use an existing executable

You can also use a compatible ArduPilot SITL executable you have already downloaded or compiled for this computer's system and processor, without rebuilding it with UXDuck. In **New simulation › Advanced › Your own build**, use **Choose executable…** in UXDuck Desktop, or enter its absolute path on the computer running UXDuck. A browser or an older Desktop window needs the path entered manually. Choose the vehicle it runs. **Frame for this build** offers Automatic, common frames and **Custom frame…** for a name from your fork. **Read frames from build** runs the executable to ask for its own list, or reads its accompanying `vehicleinfo.json`. If neither supplies a list, the common choices remain available. Keep any supplied `vehicleinfo.json` and `defaults/` files beside the executable so its own defaults can be loaded.

A build without local-only support needs **Allow network access for this build** checked before it can start. Other computers on your network may connect to it, send RC input and receive its state. The form remembers this choice with the selected executable path, including across reopening and rebuilding at that path. A different path or Reset clears it; uncheck it to revoke it for future starts. Restarting an existing simulation keeps that simulation's original choice. Advanced opens automatically when the form remembers a custom executable. Choosing a file alone does not start it or allow network access. Builds with local-only support still use it when this option is checked.

From the command line:

```sh
uxduck sim start --vehicle multicopter --binary /absolute/path/to/arducopter --frame X --allow-network
```

Omit `--allow-network` for a build with local-only support. The same explicit `allowNetwork` option is available to MCP clients for `sim_start` and `snapshot_start`; requests still use UXDuck's configuration permissions and approvals.

## Your own physics model

ArduPilot's multicopter, plane and VTOL simulators can fly a frame with the values in a JSON model file of your own: mass, motors, drag and the rest for a multicopter, aerodynamic coefficients for a plane. The frame still sets the layout and its usual parameters, so a quad's file goes on `X` or `quad`.

In **New simulation › Advanced › Physics model**, enter the file's full path on the computer running UXDuck. It works with UXDuck's downloaded simulators and with your own build. UXDuck checks that the file is JSON and copies it when the simulation starts, so a change to the file applies at the next start.

**Save to your models** keeps the path, named, in **Settings › Custom firmware › Physics models**. UXDuck keeps the path, not a copy, and says so there when the file has gone. **Saved model** then offers it in New simulation.

From the command line, put the path after the frame and a colon:

```sh
uxduck sim start --vehicle multicopter --frame "X:/Users/you/models/quad.json"
```

A physics model changes how the simulated vehicle flies, not the autopilot. To change the autopilot's own code, build your own simulator, above.

## Connect to it instead

SITL started with `--no-mavproxy` listens on TCP port 5760. Choose **Connect to it instead**, pick **TCP client** and give the simulator's address and `5760`.

## Simulated, not real

UXDuck marks the vehicle simulated when it says so itself: ArduPilot SITL and PX4 SITL have `SIM_` parameters. Its flights count as simulated in the **Logbook**. To change it, open the vehicle's page and use **Simulated**.
