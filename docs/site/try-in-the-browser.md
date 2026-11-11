---
title: Try it in the browser
search: ArduPilot simulator in the browser, nothing to install
order: 1
group: Get started
summary: Fly a simulated ArduPilot plane, VTOL or copter in Chrome, Firefox or Safari with nothing to install, then connect to UXDuck on your computer.
---

Try a simulated vehicle before connecting hardware.

1. Open [app.uxduck.com](https://app.uxduck.com) in an up-to-date Chrome, Firefox or Safari on a computer and choose **Start flying**. Nothing to install.
2. A simulated VTOL plane starts in flight over Y Combinator's parking lot in Dogpatch, San Francisco. If a simulation is already running, UXDuck keeps it instead of starting another.
3. Read [Telemetry, Cockpit and Messages](/docs/telemetry-and-control), then use **Cockpit** to try a command and see its receipt. Use **Stop simulation** there to finish.

Choose **Simulated vehicle** under **Fleet** to select another type and starting place. The simulation runs in this browser. Keep its owner tab open and visible: a browser may suspend a hidden tab. If controls disconnect, return to that tab and wait for reconnection. Failed actions are not retried. Saved snapshots and settings stay in this browser when it can keep local data. iPhone and iPad do not run browser simulations.

## Connect the browser to this computer

To connect a real vehicle or use a coding agent's MCP tools, [install UXDuck](/docs/install) and choose **Open in browser** from UXDuck's menu. Its local interface opens in your default browser, including Firefox and Safari. This keeps real vehicles and simulations running in the installed app when browser tabs are hidden.

Video availability depends on your browser and computer's decoder. If H.265 cannot play, configure the camera to send H.264; the Camera tab explains an unsupported codec.

The menu at the top left shows whether you are using **Browser** or **Computer**. Check it if a vehicle appears in one place but not in your MCP session.

The in-app AI chat is currently hidden in production. You can use an external coding agent through [MCP](/docs/mcp).
