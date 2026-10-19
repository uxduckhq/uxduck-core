---
title: Use with AI agents
order: 6
group: Debug and integrate
summary: Connect Claude Desktop, Claude Code, Codex or Cursor and investigate telemetry over time.
---

Connect an AI agent to UXDuck on this computer to read vehicle telemetry, parameters and messages. The in-app AI chat is currently hidden in production; MCP works through your external agent.

<!-- mcp-setup -->

## Claude Desktop

1. In Claude Desktop, open **Settings › Developer › Edit Config**.
2. Add `uxduck` under `mcpServers` as shown above, keeping any servers already there.
3. Quit and reopen Claude Desktop.

On Windows the file is `%APPDATA%\Claude\claude_desktop_config.json`; use `"command": "uxduck"`.

## Check the connection

Start UXDuck yourself by opening the app or running `uxduck daemon start`, then
connect or reconnect the agent's MCP connection. MCP never starts UXDuck
automatically. If UXDuck is offline, MCP asks you to start it and reconnect.
Closing the MCP connection leaves UXDuck and its work running. Ask: **“List the vehicles UXDuck can hear, and show the running version and link status.”** It should use `vehicle_list`, `daemon_about` and `link_list`.

An empty vehicle list means the connection reached UXDuck but has no vehicle to inspect. Follow [Connect a vehicle](/docs/connect-a-vehicle) or [Troubleshooting](/docs/troubleshooting). A simulation running only inside a browser is separate from the installed UXDuck that `uxduck mcp` connects to.

## Keep the agent's context small

`uxduck mcp` offers two tools at connection time. `uxduck_search_tools` finds
vehicle tools by name or words and returns their full argument schemas;
`uxduck_call_tool` runs one with its name and arguments. The vehicle tools named
below are found this way. The agent loads only the schemas it needs, even in a
client that loads every advertised MCP tool at startup. All vehicle tools still
come from the same API contracts.

For example, search with `{"query":"vehicle_list"}`, then call with
`{"name":"vehicle_list","arguments":{}}`. Exact names return one schema. Word
searches return three matches by default, at most five; `nextOffset` continues the
same query without returning the whole catalogue. Reuse a schema once found.
Discovery reads no telemetry and performs no vehicle action. Calls keep the same
permissions, approvals, results and structured errors as direct vehicle tools.

For a client or integration that needs every tool listed individually, including
client-side per-tool allow/deny rules, run `uxduck mcp --all-tools` instead.
Existing direct tool names remain callable in
both modes. Reconnect the MCP client after changing modes or updating UXDuck.
Exact token savings depend on the client and model; discovered schemas and tool
results also consume context. Use filters, bounded time windows and small page
sizes when investigating a flight.

## What it may do

- **Reading** vehicles, telemetry, parameters and history needs no approval.
- **Commands and changes** wait for your approval in UXDuck, unless you already allowed that tool. The request offers **Allow once**, **Allow for this connection**, **Always allow** and **Deny**. An unanswered request is denied after 60 seconds.

Answer from a terminal:

```sh
uxduck approvals
uxduck approvals allow <id>
uxduck approvals deny <id>
```

Change tool permissions in **Settings › Agents and tools**. Read access can inspect what is happening without arming, changing modes or writing parameters.

## Investigate a vehicle

Start with `vehicle_list`, `telemetry_get`, `vehicle_messages` and `vehicle_history`. Useful questions include:

- “Why won't this vehicle arm? Read its latest pre-arm messages and relevant parameters; explain the evidence before proposing a change.”
- “This altitude looks wrong. Compare the home, sea-level and terrain frames, their units and their timestamps.”
- “Is the vehicle still sending telemetry? Compare heartbeat age, link status and the age of the raw messages.”

Use `param_get` to read a parameter's actual value and `describedBy` before interpreting it. [Parameter descriptions](/docs/parameters#missing-descriptions) must match the firmware; a description from another version can mislead.

## Compare telemetry over time

Ask: **“Summarise battery voltage, GPS fix and EKF health over the last 60 seconds. Show the coverage and missing signals, and correlate any changes with vehicle messages.”**

`telemetry_window` reads selected signals already captured in memory. It retains up to five minutes, subject to memory limits, and reports timestamps, units, numeric extrema and text-state summaries. It is useful for voltage dips, changes in GPS fix or rising vibration while you reproduce a problem.

The result includes coverage, missing keys and `historyTruncated`. Returned samples and distinct text states are bounded separately. A short window after starting UXDuck, a restart, or a limit can leave less evidence than you requested. Ask the agent to report these limits before drawing a conclusion. Numeric extrema cover retained observations even when fewer samples are returned.

`mavlink_inspect` shows raw-message freshness and rates; `mavlink_history` shows retained raw messages. `vehicle_messages` is a snapshot: call it again for newer messages. These tools do not analyse onboard DataFlash or ULog files, and capture can miss brief events between updates.

## Diagnose forwarding and loops

In the app, open a vehicle’s **Details → Forwarding → View evidence** for the same recorded routing and loop evidence. Local port owners and the last loop’s time appear beside the destination; expand packet details, socket details or daemon logs for technical evidence. The forwarding rows on Fleet have the same controls.

Ask: **“Explain why forwarding stopped for this vehicle. Resolve the connection in the warning, show the packets that triggered the decision, and identify the local program at the destination.”**

| Tool              | Evidence                                                                                                                                                                                                  |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `link_diagnose`   | Resolves an exact vehicle/link name or ID, including IDs from warnings, to its address, status, vehicles, forwarding destinations and last preserved loop incident. Ambiguous names require an ID.        |
| `router_events`   | Recent forwarding and loop-drop decisions, source/destination peers, matched packet, threshold and disabled forward. Loop shutdown evidence survives a daemon restart.                                    |
| `mavlink_history` | Query by `vehicleId` or `linkId`, direction and message `id`; use `allMessages` for every message type. Detailed queries include sequence numbers, exact-wire fingerprints and available peer provenance. |
| `socket_inspect`  | Visible local owners of a UDP/TCP port, with process names and PIDs. `available`, `complete` and notes describe platform or permission limits.                                                            |
| `daemon_logs`     | Recent redacted log lines, filtered by level, component or a literal substring.                                                                                                                           |

Packet, router and log queries accept `fromUs`/`toUs` as Unix microseconds, default to the last minute, and allow windows up to ten minutes. Pages contain 50 entries by default, up to 200; pass the returned `older` as `before` with the same filters to continue. The cursor keeps the original window. `includeRaw: true` includes packet bytes as hexadecimal in packet and router results; bytes are omitted by default. Fingerprints help compare exact wire packets and are not authentication.

Inspect `coverage` and its notes before drawing a conclusion. The installed daemon keeps recent provenance and outbound packets in memory; older received packets come from tlogs, whose format contains no peer addresses. Normal routing and logs have bounded memory retention. Preserved loop evidence retains the original addresses and bytes, even after restart. A router selecting a destination or queuing a send does not prove network delivery.

These diagnostics read existing evidence without sending commands, probing remote hosts, opening sockets or changing forwarding. Native packet history, router capture, process ownership and logs require installed UXDuck; browser simulations report unsupported sources explicitly.

## If setup fails

1. Run `uxduck --version` and `uxduck status` in a terminal. If `uxduck` is not found, finish [installation](/docs/install) and restart the coding agent so it sees the updated PATH.
2. Check that the MCP command uses the same `--dev` or `--data-dir` option as the running UXDuck. For example, use `uxduck --dev mcp` for a development installation.
3. After updating UXDuck, restart it when you are ready, then reconnect MCP. The running version from `daemon_about` is the one that supplies the tools; replacing the CLI alone does not update a running process.

MCP uses standard input and output; add it as an MCP server rather than running it as an ordinary chat command.
