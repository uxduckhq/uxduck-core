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

Every type's defaults end with the vehicle's health: RC RSSI, Telemetry radio, EKF, vibration and battery used. RC RSSI uses the autopilot's configured RSSI source, usually the receiver. Its device-dependent scale is shown unchanged, without a percentage or generic warning thresholds. Telemetry radio uses the radio's own status report; availability depends on the hardware, firmware and connection. A dash means no reading is available, rather than a modem fault. The desktop header uses RC RSSI by default. EKF turns amber at 0.5 and red at 0.8, and vibration at 30 and 60 m/s². Planes add wind, multicopters the rangefinder's height, and submarines leaks and water temperature. A value the vehicle never sends is left out.

Altitude is three tiles: **Altitude above home** in every flying type's defaults, and **Altitude above sea level** and **Altitude above terrain** from **Edit layout…**. Above terrain comes from the terrain data the vehicle holds, so it shows a dash until the vehicle has data for where it is. Give an altitude tile a name of your own and its frame moves beside the number.

**Edit layout…** changes them, their order and their colour rules; editing UXDuck's default saves it as your own. Layouts follow you to your other computers with sync.

## Cockpit

Everything that acts on the vehicle: its mode, arming, take-off, landing, return and the rest, each with its receipt, and, in preview builds, plugins' buttons. A simulated vehicle also has **Pause**, **Save snapshot** and **Stop simulation**. **Pause** freezes it where it is until you choose **Resume**; **Shift+P** does the same. A stopped simulation stays in the sidebar as Stopped, with its name, history and tabs, also after UXDuck restarts; **Start simulation** starts it again from where it first started. In its **…** menu, **Simulation › Stop and hide** hides it until you start it again, and **Delete simulation…** removes it with its history after asking. A real vehicle can't pause, so the Cockpit says what holds one in place instead. A practice exercise can't be paused. The **…** beside its name is the same menu as in the sidebar.

While the vehicle holds a mission, the Cockpit's **Mission** subtab (**Shift+M**) shows how far through it the vehicle is, and while it runs, what's left: "Running step 3 of 6 · 1.2 km, about 4 min left", at the plan's speeds. Under that, **Step** lists the mission's steps by number and command, such as "4 · Waypoint", with the current one marked. Choose one and press **Go to step**: in AUTO the vehicle goes on to it at once, and in any other mode it is where the mission goes on from once it is in AUTO. **Restart mission** sends it back to step 1 and resets its repeats, so a finished mission can fly again. In flight it is held, because the steps already flown are flown again. On ArduPilot, **AUTO** under the steps says what switching to AUTO does: it goes on from the current step, or starts again from step 1 (`MIS_RESTART`), and the Cockpit says so when you choose a later step. **Clear it when it restarts** sets whether the vehicle forgets its mission at each boot (`MIS_OPTIONS`). The palette has **Restart Alpha's mission** and **Go to a step of Alpha's mission** too. On the Missions page, each step of the vehicle's mission has **Go to**, and right-clicking one of its waypoints on the map offers **Go to step 3 · Waypoint**.

A copter flying its mission in AUTO has **Pause mission** beside the progress: it holds where it is, still in AUTO, and the line reads "Mission paused". **Continue mission** goes on from the same step. Planes, rovers and PX4 vehicles pause by changing mode instead. When the mission has a landing sequence (a `DO_LAND_START`), **Land via mission** sends the vehicle to it, to land along the planned approach instead of RTL's straight line.

Outside AUTO, **Fly from step 3** makes the chosen step current and starts the mission in one press. You hold it, because on the ground it arms the vehicle first. On the ground an ArduPilot copter or plane leaves only from a take-off step, and with `MIS_RESTART` at 1 AUTO always starts from step 1, so the button says so instead.

After an RTL, a battery change or a reboot, the subtab says which step the vehicle last reached, with **Resume at step 4**, the step after it. Pressing it chooses that step, and then **Fly from step 4** or **Go to step** goes on from there.

The vehicle's History says how far through its mission each flight got, such as "reached mission step 4 of 6", and a replay's timeline details have a **Steps** lane with a tick at each step reached.

For sticks, switches and gamepad buttons, follow [Controllers and radio transmitters](/docs/controllers).

## Messages

Everything the vehicle says, such as `PreArm: Throttle below failsafe`, in the order it came. Filter by severity, or search.

Only warnings and worse become notifications; **Vehicle messages that notify** in **Settings › UXDuck on this computer** changes that. The newest message shows briefly at the foot of the sidebar; choose it to see it in Messages.

## Instruments

Speed, altitude and heading as large numbers, each with a thin rail showing where it will be in about five seconds, over a quiet horizon. The altitude is the one chosen in **Settings › Altitude**, above home unless you change it, with its frame named beside it. A ring with wings shows where the vehicle is going, so drift in the wind or a boat's slip shows at once. A chip appears only when the battery, GPS, EKF, vibration or radio is past its threshold, the worst first. Rovers and boats leave out the pitch ladder and climb and show the next waypoint; submarines show depth.

## Instruments (classic)

Open it from **+** or the input, or, in a build with AI chat enabled, ask the agent to show the classic instruments. It draws the vehicle's flight instruments as Mission Planner does: the horizon with its pitch ladder and roll arc, speed on the left (airspeed for planes, groundspeed otherwise), altitude in the frame chosen in **Settings › Altitude** with the frame named under its value, climb on the right, and the heading across the top. **DISARMED** shows in red until it arms, then a timer runs. Below are the mode, battery, GPS and the waypoint with its distance.

If UXDuck loses contact with the vehicle, the instruments grey out and say so.

The on-screen displays over a camera and over the 3D map show altitude in the frame **Settings › Altitude reference** chooses. To show them another way, open the display's menu (the gauge button) and pick under **Altitude**: above home, above mean sea level, or above terrain. The choice holds for every on-screen display, and **As in Settings** follows Settings again. Above terrain needs the vehicle's terrain data; until it has some, the display falls back to another frame and names it.

### Camera behind the instruments

Either instruments tab can show the vehicle's camera behind it, as an FPV goggle does: open the tab's vehicle menu in its tab bar and turn on **Camera behind instruments**. The camera is the one its **Camera** tab shows. The horizon stays painted in a disc in the middle, since the camera may not point where the vehicle does, and each value keeps a dark backing so it reads over any picture. If the camera stops, its last picture stays with **Video stopped** in the corner. A vehicle with no camera shows the plain instruments.

## Check freshness before diagnosing

A colour threshold is a display rule, not a diagnosis. Check the value’s unit, source and timestamp, and compare it with the vehicle’s messages. A dash means no value is available; it does not mean zero. A recent heartbeat does not prove every sensor value is fresh.

For stale values, connection problems or rejected commands, follow [Troubleshooting](/docs/troubleshooting). To compare a voltage dip or a GPS change over time, use the [MCP telemetry window](/docs/mcp#compare-telemetry-over-time).
