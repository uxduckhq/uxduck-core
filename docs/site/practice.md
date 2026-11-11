---
title: Practice
search: Learn to fly a drone with simulated practice exercises
order: 13.6
group: Get started
summary: Get started in the sidebar, then short exercises on a simulated vehicle, judged as you fly.
---

Practice gives you short exercises on a simulated vehicle, so you get used to the controls before flying a real one.

## Get started

**Get started** in the sidebar, above Fleet, is a checklist. Its first item is a first flight with your simulated vehicle, with the steps under it: send it somewhere, change its mode, bring it back. Each ticks once the vehicle has done it. After that come a practice flight, another kind of vehicle and more. The sidebar shows the one to do now; **See all** opens the whole list on Home. To hide it, choose **Hide Get started** from its **⋯** menu, or **Hide from the sidebar** on Home. **Show in the sidebar** on Home, **Get started in the sidebar** in Settings › General, or **Show Get started** in the command palette brings it back.

## Try one

1. Start a simulated VTOL plane with **Start flying**, or a multicopter with **New simulation**. The tab lists the exercises that fit the vehicle.
2. Open **Practice** from **+** in any tab bar.
3. Choose an exercise and press **Start**. The vehicle starts again where the exercise needs it, then the exercise begins.
4. Do what each step says, in the Cockpit tab and on the map beside it. A hint appears if a step takes a while.

You see one result at the end, such as "Passed. Home with 31% left.", and your best result is kept. **Give up** ends an exercise; the vehicle stays where it is.

## The exercises

- **Read the panel**: answer three questions from Telemetry.
- **Bring it home**: it starts 400 m out with a low battery; get it home.
- **Mode tour**: Loiter, Guided, Return to launch, then Land.
- **Fly there**: send it to three rings in turn with Go to.
- **Land in the circle**: land inside the ring.

Once you know the basics:

- **Plan the survey**: plan a mission over the field on the Missions page, upload it and fly it in Auto.
- **Timed delivery**: from the ground, reach the drop point, stay 20 s, and land at home within 4 minutes.
- **Hold the box**: by hand, keep it in the box for a minute in Loiter, then in Stabilize. On the VTOL, QLOITER then QHOVER.
- **Through the gates**: by hand, fly through five gates in order. On the VTOL and the plane, in FBWB, with the gates 300 m out.
- **Parameter puzzle**: it is too slow to reach the ring in time; find the parameter and fix it.

## Flying by hand

The exercises flown by hand need a controller or your keyboard flying the vehicle; **Open Cockpit** in the exercise opens the Cockpit's **Controls** subtab with the keyboard picked under **Fly with**; pick a controller there instead to fly with it. Get started's **Fly it by hand** opens the first of them.

A vehicle in a mode that flies itself, such as GUIDED or a plane's LOITER, barely answers the sticks. While a controller or the keyboard flies it, the Cockpit's status line names it (pressed, it opens **Controls**), and **Controls** says so in one line with **Open Modes**: the assisted and manual ones are those you steer, such as QLOITER (it hovers and holds its place) on the VTOL or LOITER on a multicopter. Disarmed, the line says to arm it next, or, in a mode that flies itself, to choose a mode to steer first. Nothing changes the mode until you choose one.

Only commands you send count, not the agent's. Exercises never run on a real vehicle.

From a terminal: `uxduck practice list`, `uxduck practice start mode-tour --vehicle "Sim 1"`, `uxduck practice status`. `uxduck dev replay flight.tlog --practice bring-it-home` judges a recorded flight again and prints each step and the result.
