---
title: Practice
order: 13.6
group: Get started
summary: Get started in the sidebar, then short exercises on a simulated vehicle, judged as you fly.
---

Practice gives you short exercises on a simulated vehicle, so you get used to the controls before flying a real one.

## Get started

**Get started** in the sidebar, above Vehicles, is a checklist. Its first item is a first flight with your simulated vehicle, with the steps under it: send it somewhere, change its mode, bring it back. Each ticks once the vehicle has done it. After that come a practice flight, another kind of vehicle and more. The sidebar shows the one to do now; **See all** opens the whole list on Home. To hide it, choose **Hide Get started** from its **⋯** menu, or **Hide from the sidebar** on Home. **Show in the sidebar** on Home, **Get started in the sidebar** in Settings › General, or **Show Get started** in the command palette brings it back.

## Try one

1. Start a simulated VTOL plane with **Start flying**, or a multicopter with **New simulation**. The tab lists the exercises that fit the vehicle.
2. Open **Practice** from **+** in any tab bar.
3. Choose an exercise and press **Start**. The vehicle starts again where the exercise needs it, then the exercise begins.
4. Do what each step says, in the Control tab and on the map beside it. A hint appears if a step takes a while.

You see one result at the end, such as "Passed. Home with 31% left.", and your best result is kept. **Give up** ends an exercise; the vehicle stays where it is.

## The exercises

- **Read the panel**: answer three questions from Telemetry.
- **Bring it home**: it starts 400 m out with a low battery; get it home.
- **Mode tour**: Loiter, Guided, Return to launch, then Land.
- **Fly there**: send it to three rings in turn with Go to.
- **Land in the circle**: land inside the ring.

Once you know the basics:

- **Plan the survey**: plan a mission over the field on the Mission tab, upload it and fly it in Auto.
- **Timed delivery**: from the ground, reach the drop point, stay 20 s, and land at home within 4 minutes.
- **Hold the box**: with a controller, keep it in the box for a minute in Loiter, then in Stabilize.
- **Through the gates**: with a controller, fly through five gates in order.
- **Parameter puzzle**: it is too slow to reach the ring in time; find the parameter and fix it.

Only commands you send count, not the agent's. Exercises never run on a real vehicle.

From a terminal: `uxduck practice list`, `uxduck practice start mode-tour --vehicle Alpha`, `uxduck practice status`. `uxduck dev replay flight.tlog --practice bring-it-home` judges a recorded flight again and prints each step and the result.
