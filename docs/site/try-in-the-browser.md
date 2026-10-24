---
title: Try it in the browser
search: ArduPilot simulator in the browser, nothing to install
order: 1
group: Get started
summary: Fly a simulated ArduPilot plane, VTOL or copter in Chrome with nothing to install, then connect to UXDuck on your computer.
---

Try a simulated vehicle before connecting hardware.

1. Open [app.uxduck.com](https://app.uxduck.com) in Chrome and choose **Try a simulation**. Nothing to install. If UXDuck was connected but stopped, the button is **Start simulation**.
2. A simulated VTOL plane starts in flight over Y Combinator's parking lot in Dogpatch, San Francisco. If a simulation is already running, UXDuck keeps it instead of starting another.
3. Read [Telemetry, Cockpit and Messages](/docs/telemetry-and-control), then use **Cockpit** to try a command and see its receipt. Use **Stop simulation** there to finish.

Choose **Simulated vehicle** under **Fleet** to select another type and starting place. The simulation runs in this browser; keep its page open while using it.

## Connect the browser to this computer

To connect a real vehicle or use a coding agent's MCP tools, [install UXDuck](/docs/install). Choose **Connect to UXDuck on this computer** on the home page or in **Settings**.

Chrome may ask to let the page reach UXDuck on this computer. UXDuck then shows the same six-digit code with **Allow**. Once allowed, the page reconnects automatically. Safari cannot reach UXDuck this way; open the app from UXDuck instead.

The menu at the top left shows whether you are using **Browser** or **Computer**. Check it if a vehicle appears in one place but not in your MCP session.

The in-app AI chat is currently hidden in production. You can use an external coding agent through [MCP](/docs/mcp).
