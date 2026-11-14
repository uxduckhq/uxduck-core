---
title: Tuning
search: PID tuning, gains, rates and feel for ArduPilot and PX4 copters, planes and VTOLs
order: 14.5
group: Use your vehicle
summary: Change how an ArduPilot or PX4 copter, plane or VTOL feels to fly and its gains per axis, watch how it follows on a live graph, and jump between snapshots.
---

Change how a vehicle feels to fly and its gains, between flights or in the air, and go back to any earlier tune. Tuning is for ArduPilot and PX4 copters, planes and VTOLs. Open it from the vehicle's **Setup**, its menu's **Open tuning**, or the command palette.

A line at the top says where the tuning stands: whether the vehicle has its default tuning, how many of its tuning values differ from the defaults, which snapshot it has, and what is staged but not written yet. Under it, **Undo** goes back to the snapshot before the last change, **Snapshot** keeps the values now, and **History** lists every snapshot.

New to tuning? Start with **Steps**, then **How it flies**: move the sticks, read the line under the graph, and press the change it suggests. Write it, fly again, and see what it did.

## Steps and Prepare

**Steps** is ArduPilot's tuning process for the vehicle. A copter, or a VTOL's hover, has Prepare, First flight, Filters, Tune and Feel; a plane has Prepare, Tune and Feel. Each step says whether it is done, worked out from the vehicle's parameters, and when it was last changed, from its history. The first step not done is marked **Next**. Each has one button that takes you to that part of Tuning or of Parameters, and **ArduPilot's guide** opens ArduPilot's page on it. The steps are advice: any of them can be skipped, done out of order or done again.

**Work out starting values**, in Prepare, asks for the prop size in inches, the battery's cells and its type (LiPo, LiPo HV or Li-ion). It works out what ArduPilot's "Setting the Aircraft Up for Tuning" gives: the battery's full and empty voltages, the thrust expo, the gyro and accelerometer filters, the acceleration limits and the rate filters, each in the unit the vehicle's own parameter uses. They are listed against the vehicle's values, each with why. Values already right are listed by name only. A value someone has changed from its default starts unticked, and says so, so it is never replaced unasked. **Stage** stages the ticked ones as one change; nothing reaches the vehicle until you write it. A thrust stand measures the thrust expo better than the guess for its prop size.

## AutoTune

**AutoTune** is ArduPilot's own tuning in flight: the vehicle twitches each axis and works out its gains. You fly it; UXDuck says what to check, how to enter it, what it is doing and what it changed. A copter tunes in AutoTune, a VTOL's hover in QAUTOTUNE, and a plane in AUTOTUNE.

**Before you fly** lists Prepare and Filters from Steps, the wind the vehicle estimates (calm is 5 m/s or less) and its battery (start on a full one: an axis takes a few minutes). **Axes it tunes** and **How hard it pushes** stage changes to `AUTOTUNE_AXES` and `AUTOTUNE_AGGR` (a plane's `AUTOTUNE_LEVEL`), written like any other. The choice at ArduPilot's default says so, as the vehicle's firmware reports it, since the default changed between releases (Copter 4.6 lowered `AUTOTUNE_AGGR` to 0.075). **How to enter** says which mode switch position or switch enters it now; where none does, it offers to stage AutoTune on a mode switch position of your choice, showing the mode that position has now. Once the vehicle flies, **Switch to AutoTune now…** switches to it when you confirm. UXDuck never starts AutoTune by itself, and the agent can't start it.

**Start** keeps the gains as they are now as a snapshot, "Before AutoTune", and says how to fly it. While it runs, one large line says the axis and stage, such as "Roll: raising rate D, 50%", or that it is paused while you move the sticks, and How it flies, below it, draws the axis. When it ends, the gains before and now are side by side, with the acceleration limits and the other values AutoTune saves, with what the firmware does with them: a copter keeps the new gains when it lands and disarms in AutoTune, and a plane keeps each axis once it finishes. UXDuck reads the parameters again, since the vehicle saves them without saying so, and the vehicle's history gets an entry with each gain before and after. **Go back to Before AutoTune** stages the gains from before.

On a VTOL or copter with scripting, ArduPilot's QuikTune script tunes in flight too: **Open Setup's Scripting** goes to its row.

## Feel

Feel changes how the vehicle is asked to move, not how hard it corrects: how quickly it follows the sticks, how far it leans or banks, how fast it may roll, pitch and turn, and how fast it climbs and descends. Each control has a slider between two words, such as **Smooth** and **Sharp**, and its number. ⓘ says which parameter it writes and in which unit. On ArduPilot, a maximum rate of 0 is **No limit**, at the fast end of its slider. A descent speed of 0 is **Same as climb speed**, and its slider rests where the climb speed's does.

## Gains

Gains shows the vehicle's rate controllers: a row for each term and a column for Roll, Pitch and Yaw, with Angle P above them. Each term says what it does in a few words: P pushes back, I holds against drift, D brakes, FF leads the sticks. Gains show as plain numbers, without their units. **Filters and limits** adds Max I and the filters. While Roll and Pitch have the same values they are linked, so a change to one is a change to both; **Roll and Pitch together** unlinks them. **Scale** multiplies P, I and D on every axis together, from half to double, and keeps their ratios. A filled dot beside a value means it is not the firmware's default; a ring means it differs from your first snapshot. Choose either to stage that value again. A VTOL shows its hover gains and its plane gains separately.

## The response graph

**How it flies** draws, for the axis you choose, the rate the vehicle was asked for against the rate it reached, and below them the P, I, D and FF that made it. Move the sticks or fly a few turns to see how it follows. A dashed line marks each tuning value changed while it runs, with its old and new value, so you can see before and after side by side; the changes are listed under the graph too. Drag across the graph to look at a stretch, and double-click it to go back to live.

Each axis' button says its reading in a word or two. Under the graph, one line reads the last 10 seconds: **Following well**, **Slow to follow**, **Overshooting**, **Oscillating at about 5 Hz**, or **Not enough movement to tell** when the sticks were still. ⓘ shows its numbers. Under it, a line says what to try next, with a button that stages that change, such as **Try Roll and Pitch P 0.149**: more P when it lags, less when it overshoots or shakes, and less D when it shakes very fast. The button only stages the change; nothing reaches the vehicle until you write it. While a change is staged, or for ten seconds after a gain changes, it waits for you to write it and fly again instead of suggesting another.

ArduPilot sends this data for the axes set in `GCS_PID_MASK`. When an axis is not set, Tuning asks once, for every axis, before it turns it on, and the change is kept in the vehicle's history like any other. When you close the tab, Notifications offers to switch it off, and **Switch off** puts the mask back; the offer is kept if the app is reloaded. While the graph shows, UXDuck asks the vehicle for its data 25 times a second, and for its usual rates again when it closes. Without that data the graph uses the vehicle's attitude rates against their targets and says so; there are no P, I, D and FF then. A replay draws the graph from what its recording kept.

## On PX4

A PX4 vehicle has Feel, Gains, How it flies, snapshots and All tuning parameters. Its gains are PX4's own: `MC_` for a multicopter and a VTOL's hover, and `FW_` for a plane. Steps, Prepare and AutoTune follow ArduPilot's process, so a PX4 vehicle doesn't have them. PX4 sends no P, I, D and FF data, so How it flies draws its attitude rates against their targets. PX4 limits a rate of 0 to 0 rather than lifting the limit, so its rate sliders have no No limit end and stop short of 0. PX4 doesn't report its defaults, so the line at the top doesn't compare the tuning with them.

## Ask the agent

With Tuning open while the vehicle flies, ask the agent why it wobbles, drifts or feels slow. It reads the gains, Steps, the last AutoTune and how each axis flew, says what it found, opens Tuning at that part, such as Roll's graph, and proposes one change with its reason. Nothing is written until you approve it. The agent never starts AutoTune: you fly it. Claude Code and other MCP clients read the same through `tuning_state`.

## Write and snapshots

Every change is staged as in Parameters and written with **Write**, with a reason if you give one. Before the first change is written, UXDuck keeps a snapshot of the tuning values, "Before tuning". After each write it keeps another, named by what changed, such as "Roll P 0.135 → 0.15", with your reason as its note. A snapshot holds only tuning parameters, and the same values are never kept twice.

**History** lists them newest first, and says which one the vehicle has now. **Snapshot** keeps the values now, with a name and a note if you want. **Edit** changes a name or a note. **Jump**, and **Undo** at the top, show what would change, by name such as Roll P, then write only those tuning values; what the vehicle had before is kept as a snapshot first, so you can jump back. Nothing outside tuning is touched.

**All tuning parameters** lists every parameter the tab writes. Open one in Parameters to see its description and history.
