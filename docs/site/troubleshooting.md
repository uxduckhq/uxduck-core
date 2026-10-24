---
title: Troubleshooting
search: Troubleshoot ArduPilot connections, stale telemetry and pre-arm failures
order: 5.5
group: Debug and integrate
summary: Diagnose missing vehicles, stale telemetry, ArduPilot pre-arm failures and missing parameter descriptions.
---

Start with the connection, then the age of the evidence, then the vehicle's messages. These checks read state without changing flight settings.

## The vehicle is missing

1. Check whether the app is connected to **Browser** or **Computer** in the top-left menu. A browser simulation is separate from the installed UXDuck used by CLI and MCP.
2. Run `uxduck status`, `uxduck ports`, `uxduck rpc call link/list` and `uxduck rpc call vehicle/list`. For a development installation, add `--dev` to each command.
3. Match the link to the sender: serial port and baud rate, UDP destination and listening port, or TCP host and port. Follow [Connect a vehicle](/docs/connect-a-vehicle). Check `ports` for another program holding the listener.
4. If the link is open but there is no vehicle, check that the autopilot or simulator is sending MAVLink heartbeats to that endpoint. Opening a port alone does not establish a vehicle connection.

## A value has stopped updating

Use the Telemetry value's information button to check its source and unit. In MCP, compare `telemetry_get` with `mavlink_inspect`, `link_list` and the vehicle's `heartbeatAgeMs`.

- An old heartbeat and old sensor messages suggest a connection or sender problem.
- A fresh heartbeat with old sensor messages points to a particular stream or sensor. Check whether that message type is actually arriving before changing stream rates.
- A missing value or a dash is missing evidence, not a reading of zero. Above-terrain altitude needs terrain data; above-home and sea-level altitude use different reference frames.

Repeat the read while reproducing the symptom, or use [telemetry over time](/docs/mcp#compare-telemetry-over-time). Check the reported coverage and limits; recent history starts when UXDuck starts capturing it.

## Arming or a command is refused

Read the command receipt first. UXDuck may have denied permission or reached an approval timeout. Check [Confirmations](/docs/confirmations).

If the vehicle rejected it, read **Messages** or MCP's `vehicle_messages` for the latest `PreArm` or other explanation. Check that it is recent and belongs to the selected vehicle. Read the relevant parameter's current value and matching firmware description before proposing a change. An accepted receipt still needs telemetry to confirm the result.

For intermittent GPS, EKF, vibration or battery problems, correlate messages with the relevant signals over the same interval. A current green tile alone cannot explain an earlier failure.

## Parameter descriptions are missing

Run `uxduck params definitions list` and follow [Missing descriptions](/docs/parameters#missing-descriptions). Values and descriptions are separate downloads. A public catalogue verification key belongs in the published build; it is not a secret the user must supply. Custom firmware needs descriptions from its matching commit.

## Camera video is blank

Run `uxduck video list`. Use `uxduck video probe --port 5600` to inspect the arriving stream, replacing `5600` with its port; `uxduck video probe --help` lists the options. Check its transport and codec against [Camera video](/docs/camera-video). QGroundControl and UXDuck cannot both listen on the same UDP port on this computer.

## Prepare a useful report

Include the running version and release stage from `uxduck rpc call daemon/about`, the vehicle's firmware version, the link type, the exact symptom and when it happened. Add the recent message or command receipt and whether telemetry was fresh. For an MCP investigation, include the requested interval, actual coverage, missing keys and truncation flags.

Review screenshots and command output before sharing: they can contain vehicle locations, network addresses and other details of your setup. Onboard DataFlash and ULog analysis is not available through these MCP tools yet.
