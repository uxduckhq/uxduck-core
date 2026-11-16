---
title: Replay a flight
search: Replay a flight, a tlog, a DataFlash .bin log or a PX4 .ulg log with its video, on a timeline
order: 7.5
group: Use your vehicle
summary: Play a flight back on both maps and in every tab, with its video, from the Logbook, a tlog, a .bin or .ulg log or your account.
---

A replay plays a flight back: its vehicle flies its path on both maps, every tab shows it as it was, and its camera's recordings play at the same time. Nothing can be sent to a replay vehicle, and nothing is recorded of it.

## Open one

- **Logbook**: click a flight, or its play button. While a vehicle is live, a click only selects the flight, so the play button starts its replay.
- **A file**: Logbook › **Open a log…**, or drop a telemetry log (`.tlog`), a DataFlash log (`.bin`) or a PX4 log (`.ulg`) on the window. Mission Planner's, QGroundControl's and MAVProxy's telemetry logs all open.
- **Files**: a DataFlash log (`.BIN`) or a PX4 log (`.ulg`) downloaded from the vehicle › **…** › **Replay**.
- **An SD card**: plug in the vehicle's card and the Logbook offers to copy its DataFlash or PX4 logs.
- **Your account**: Logbook › Telemetry logs in your account › a log › **Replay**, in UXDuck Desktop or in UXDuck Web with nothing installed.
- **A terminal**: `uxduck replay` (the replay open now, else the last flight), `uxduck replay <flight id>` or `uxduck replay flight.tlog --play --speed 5`; `uxduck replay --restart` plays the open one from its start again.

In UXDuck Web, a flight flown in the browser replays from the **Logbook** on later visits too: the browser keeps the newest 96 MB of telemetry logs, and a private window keeps none. **Settings › Storage** shows how much they take and clears them.

## PX4 logs

A PX4 log (`.ulg`) replays like a DataFlash log: its position, attitude, GPS, batteries, motors, home, wind, VTOL state and modes play on the maps and in every tab, on GPS time when it had a fix. Its parameters come with their airframe's defaults, and its messages and events show as PX4 sent them. Every topic it logged, such as `battery_status` or `vehicle_attitude`, can be added to a chart at the rate it was logged. A PX4 log holds no mission, so the replay shows none. Its messages include some from PX4's console that a live link never sends.

## Save a flight's telemetry log

A flight's **…** › **Download its telemetry log** saves it as a `.tlog` file, from just before it armed to just after it ended. Mission Planner, QGroundControl and UXDuck open it.

## The bar

The bar at the foot plays and pauses, sets the speed (¼× to 16×; `--speed` in a terminal takes any speed between) and moves through the flight. At the end it stays there and the Play button becomes **Restart**. Its marks show arming, the time in the air, the modes, warnings, the settings changed and where there is video.

A flight plays from 10 seconds before it armed to 10 seconds after it disarmed. Turn off **Armed only** on the bar to play the time before and after it too, from the end of the flight before (or the log's start) to where the next flight's lead-in begins. The choice is kept on this computer, so the Logbook opens flights the same way until you turn it back on.

Keys: Space plays and pauses (restarts at the end), ← and → move 5 seconds (J and L 10, Shift 30), `,` and `.` step a frame, Shift+< and Shift+> change the speed, Home and End go to the ends. While a map has the focus the arrows pan it instead.

**Live** pauses the replay and shows a live vehicle, keeping your place. **Stop replay** ends it: at the right end of the bar, or the ■ on the replay's row in the Fleet. A replay of several vehicles stops as one, so they all leave the Fleet together. For a few seconds after, **Undo** in the sidebar brings it back paused where it was.

## Parameters in a replay

A replay vehicle's **Parameters** tab shows its values as the log had them at the replay's time, from a parameter download in the log or the one before the flight. One line above the list says how many there are, such as "All 1,104 · as at 10:15:02" or "1,050 of 1,104". Click it to see each setting changed in the log, with who changed it, and how many values were first read later in the log; those are marked **later** in the list. Values seen only before the vehicle restarted are left out, because they may not be current. Vehicle statistics, such as flight time and boot count, are folded into their own section at the end; open it and choose **Show among the others** to list them with the rest.

An agent reads a replay vehicle's parameters at the replay's time too, or at a time it asks for, and `param_changes` lists what changed in the log, with statistics left out unless it asks.

## Add a video

A screen recording of your simulator or a phone's video of the flight plays with the replay too. Choose **Logbook › … › Add a video…**, or **Add a video…** on a replay's Camera tab, and pick the file; it stays where it is. MP4, MOV and MPEG-TS work, H.264 and HEVC as they are and MPEG-2 converted once. UXDuck places it by the KLV time stamps when the video has them, else by the time the file says it began, else at the arming.

To line it up, pause where the video shows the take-off and press **This is the take-off** (or **This is the landing**), nudge it by a tenth or a whole second, or type when it starts. Each video added is its own camera, and stays where you put it.

## Several flights together

An operation's **Replay** plays all its flights on one clock. When one is a little off, nudge it in the bar's sources list; the operation keeps the nudge, so it opens matched next time.

## Save a log to your account

A log you opened from a file can be saved with **Save to account** on the bar: its flights are saved to your account and the log is uploaded as their telemetry log, so they replay on your other computers. UXDuck Web does not upload logs: use UXDuck Desktop.

## Log folders

UXDuck Desktop looks in QGroundControl's `Documents/QGroundControl/Telemetry` and Mission Planner's `Documents/Mission Planner/logs` folders. Their flights are listed in the Logbook, behind the menu beside **On this computer**, and replay straight from the file. Nothing is copied. A log the other app is still writing waits until it closes. **Settings › Log folders**, also **Logbook › Open a log… › Log folders…**, turns each folder on or off, and **Also look in…** adds another, such as MAVProxy's. A folder you add is looked into on its own, not its subfolders. Turn on **Subfolders** to look through all of it, such as a disk of logs or a custom ground station's folders. Hidden folders and links are skipped. All of it is looked through, however large, and the folder says how many entries the look has read. A folder or a log that could not be read is listed under its folder, with why, and the Logbook says so beside that folder's flights. A long list shows its first 100 and how many more there are, and **Show more** reads on. A file with a log's name that is not a telemetry log, such as the `.tlog` files a Visual Studio build writes, is listed too. No flights are missing with it, so the Logbook does not count it. QGroundControl's and Mission Planner's own folders are looked through however deep their logs are.

Besides telemetry logs, a Log folder lists ArduPilot `.bin` and PX4 `.ulg` logs, one entry for each file, dated by when the file last changed, and the `.uxflight` files UXDuck writes. Its videos (MP4, MOV and MPEG-TS) are offered by **Add a video…**, nearest the flight's time first. A folder on a disk that is not plugged in is shown as **Not plugged in**, and its flights stay listed, greyed, until you plug the disk in again.

## Flights on an external disk

To keep your computer's own disk from filling up, move flights to an external disk. In the Logbook, choose **Select**, tick the entries and choose **Move or copy to…**, or choose **Move or copy to…** in one entry's **…** menu. Choose where: your Log folders come first, then every external disk plugged in, with its free space, then **Choose a folder…**. A disk gets a **UXDuck Flights** folder, which UXDuck adds to **Settings › Log folders**. Then choose:

- **Move**: the video leaves this computer once it is on the disk. The telemetry log stays here.
- **Copy**: everything stays on this computer too.

The dialog says how much it writes and frees before anything is written. A disk without room for the video says so in its numbers and is not written to. Move deletes nothing until every file is written and read back, and only video that no other entry uses. A video you added from elsewhere stays where it was.

A moved entry stays in the Logbook marked **On T7**, with T7 being your disk's name. Its replay plays the video from the disk. With the disk unplugged it says **On T7 · not plugged in**, and its Camera tab says to plug it in.

## A flight as one file

Each entry on a disk is one `.uxflight` file. Double-click it to open UXDuck and replay the flight, with its telemetry and video in step, or drop it on UXDuck's window. A flight from another computer is added to your Logbook and plays from the file.

A `.uxflight` is an ordinary zip file, and nothing in it is compressed or locked. Inside are `telemetry.tlog`, which Mission Planner and QGroundControl replay, the camera's own video files with an MP4 copy every player plays, `flight.json` with UXDuck's details of the flight, and a `README.txt` saying the same. Rename it to `.zip` to open it with any program.
