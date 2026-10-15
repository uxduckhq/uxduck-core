---
title: The command line
order: 5
group: Debug and integrate
summary: Use read-only checks, scripts and commands from a terminal.
---

Use the command line to script UXDuck, run it without a window, or work over SSH. [Install UXDuck](/docs/install) first.

## Check the connection

These commands read state without changing the vehicle:

```sh
uxduck --version
uxduck status
uxduck ports
uxduck rpc call daemon/about
uxduck rpc call link/list
uxduck rpc call vehicle/list
```

`status` shows the data directory and whether UXDuck is running there. `ports` shows listeners and conflicts. `vehicle/list` returns the vehicle IDs used by other commands.

If you started UXDuck with `--dev` or a custom `--data-dir`, use the same option for your CLI and MCP connection. Otherwise they read a different installation's state.

## Read one vehicle

Copy a vehicle ID from `vehicle/list` in place of `VEHICLE_ID`:

```sh
uxduck rpc call telemetry/get --json '{"id":"VEHICLE_ID"}'
uxduck rpc call vehicle/messages --json '{"vehicleId":"VEHICLE_ID"}'
uxduck vehicle history VEHICLE_ID
```

For connection or stale-value problems, follow [Troubleshooting](/docs/troubleshooting). For an agent that compares readings over time, follow [Use with AI agents](/docs/mcp).

## Send a command

Choose the target explicitly when more than one vehicle is connected. Read the command's receipt to see whether UXDuck refused it, the vehicle rejected it, or the vehicle accepted it. Acceptance does not prove the action has finished: check the resulting telemetry and messages.

```sh
uxduck command --vehicle VEHICLE_ID arm
uxduck command --vehicle VEHICLE_ID disarm
```

In a terminal, a command the app would confirm, such as arm, asks first. A script is not asked, but a dangerous command, such as disarming in flight, is sent only with `--yes`.

See [Confirmations](/docs/confirmations) before issuing commands.

## Find the remaining commands

```sh
uxduck --help
uxduck sim start --help
uxduck params definitions --help
uxduck video --help
```

Each subcommand lists its flags and examples. Remote watch and control workflows are still in preview; their availability depends on the build and account configuration.
