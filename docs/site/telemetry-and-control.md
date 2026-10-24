---
title: Telemetry, Cockpit and Messages
search: ArduPilot telemetry, flight controls and vehicle messages
order: 12
group: Use your vehicle
summary: Read live telemetry, send commands from the Cockpit and see their receipts, and read the vehicle's messages.
---

These tabs show what the vehicle is doing and let you act on it.

When the first vehicle connects, the flight layout replaces **Home**, once: the map and the 3D map on top, **Instruments** and **Messages** below them, and **Cockpit** above **Telemetry** on the right. **Camera** opens beside the 3D map when the vehicle's video arrives. In development and staging builds, **Ask AI** opens the chat. The in-app AI chat is currently hidden in production. **Layouts** › **Reset layout** brings this arrangement back, and the list of tabs in **Layouts** shows or hides each one in its place.

**Layouts** › **Save layout…** keeps an arrangement under a name, such as Survey. The title bar shows the layout you last chose or saved. Once you move something, it adds "· changed". **Save to Survey** keeps the change, and **Undo changes to Survey** undoes it.

## Telemetry

One tile per value, in your units. A tile fills red, amber or green by its colour rules, such as the battery under 20 % in red. Tiles of a vehicle no longer heard turn grey with when it was last seen. Search highlights matching tiles; **Enter** steps between them. Drag a tile to move it.

Every type's defaults end with the vehicle's health: radio signal, EKF, vibration and battery used. EKF turns amber at 0.5 and red at 0.8, and vibration at 30 and 60 m/s². Planes add wind, multicopters the rangefinder's height, and submarines leaks and water temperature. A value the vehicle never sends is left out.

Altitude is three tiles: **Altitude above home** in every flying type's defaults, and **Altitude above sea level** and **Altitude above terrain** from **Edit layout…**. Above terrain comes from the terrain data the vehicle holds, so it shows a dash until the vehicle has data for where it is. Give an altitude tile a name of your own and its frame moves beside the number.

**Edit layout…** changes them, their order and their colour rules; editing UXDuck's default saves it as your own. Layouts follow you to your other computers with sync.

## Cockpit

Everything that acts on the vehicle: its mode, arming, take-off, landing, return and the rest, each with its receipt, and, in preview builds, plugins' buttons. A simulated vehicle also has **Pause**, **Save snapshot** and **Stop simulation**. **Pause** freezes it where it is until you choose **Resume**; **Shift+P** does the same. A real vehicle can't pause, so the Cockpit says what holds one in place instead. A practice exercise can't be paused. The **…** beside its name is the same menu as in the sidebar.

## Messages

Everything the vehicle says, such as `PreArm: Throttle below failsafe`, in the order it came. Filter by severity, or search.

Only warnings and worse become notifications; **Vehicle messages that notify** in **Settings › UXDuck on this computer** changes that. The newest message shows briefly at the foot of the sidebar; choose it to see it in Messages.

## Instruments

Speed, altitude and heading as large numbers, each with a thin rail showing where it will be in about five seconds, over a quiet horizon. The altitude is the one chosen in **Settings › Altitude**, above home unless you change it, with its frame named beside it. A ring with wings shows where the vehicle is going, so drift in the wind or a boat's slip shows at once. A chip appears only when the battery, GPS, EKF, vibration or radio is past its threshold, the worst first. Rovers and boats leave out the pitch ladder and climb and show the next waypoint; submarines show depth.

## Instruments (classic)

Open it from **+** or the input, or, in a build with AI chat enabled, ask the agent to show the classic instruments. It draws the vehicle's flight instruments as Mission Planner does: the horizon with its pitch ladder and roll arc, speed on the left (airspeed for planes, groundspeed otherwise), altitude in the frame chosen in **Settings › Altitude** with the frame named under its value, climb on the right, and the heading across the top. **DISARMED** shows in red until it arms, then a timer runs. Below are the mode, battery, GPS and the waypoint with its distance.

If UXDuck loses contact with the vehicle, the instruments grey out and say so.

### Camera behind the instruments

Either instruments tab can show the vehicle's camera behind it, as an FPV goggle does: open the tab's vehicle menu in its tab bar and turn on **Camera behind instruments**. The camera is the one its **Camera** tab shows. The horizon stays painted in a disc in the middle, since the camera may not point where the vehicle does, and each value keeps a dark backing so it reads over any picture. If the camera stops, its last picture stays with **Video stopped** in the corner. A vehicle with no camera shows the plain instruments.

## Check freshness before diagnosing

A colour threshold is a display rule, not a diagnosis. Check the value’s unit, source and timestamp, and compare it with the vehicle’s messages. A dash means no value is available; it does not mean zero. A recent heartbeat does not prove every sensor value is fresh.

For stale values, connection problems or rejected commands, follow [Troubleshooting](/docs/troubleshooting). To compare a voltage dip or a GPS change over time, use the [MCP telemetry window](/docs/mcp#compare-telemetry-over-time).
