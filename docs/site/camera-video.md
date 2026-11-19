---
title: Camera video
search: Drone camera video over RTSP or UDP, with KLV and recordings
order: 7
group: Use your vehicle
summary: Receive H.264 or H.265 camera video over RTSP or UDP, resolve port conflicts and find recordings.
---

Watch and record your vehicle's camera beside the map, and see on the map where it's looking.

## A webcam, USB camera or analog FPV receiver

Connect the camera, or the receiver's yellow RCA video output through a USB
capture adapter, to the computer running UXDuck. In a Camera tab choose **Add a
camera › Webcam or USB capture adapter**, choose the device and press **Connect
camera**. The adapter needs an OS-compatible driver; its product title alone does
not establish compatibility.

Use **Camera default** for webcams and USB cameras. For analog FPV, choose **PAL**
or **NTSC** to request the matching input mode, correct the 4:3 picture shape and
deinterlace each field. Camera default also lets you specify picture size and
frame rate. Some analog adapters need their input/standard configured in their
driver. In **Camera settings**, choose the vehicle by hand. **Capture settings**
changes the mode; **Pause capture** closes the device, and **Resume capture** opens
it again.

Capture and recording belong to UXDuck running in the background. **Record**
continues with every tab and window closed. Unplugging ends the current recording
segment; reconnecting the same device resumes capture and recording in another
segment. Quitting UXDuck stops capture. An adapter without a stable identity may
need selecting again after a reconnect or a change of USB port. Sound is not
captured.

```
uxduck video devices
uxduck video capture "<device id>" --standard pal
uxduck video list
uxduck video record <source id>
uxduck video record <source id> --stop
uxduck video pause <source id>
uxduck video resume <source id>
```

Local footage is encoded by UXDuck's capture helper; its capture settings are
kept with the recording. Network-camera originals keep their existing path.

## Send the stream to UXDuck

1. Set the camera to send to this computer's address on UDP port **5600**: MPEG-TS, plain or in RTP, or H.264 or H.265 in RTP as QGroundControl expects.
2. A notification says **Video from …**. When UXDuck knows whose camera it is, a Camera tab opens beside the map with it; otherwise choose **Open video**.

A camera that the autopilot announces shows up under its vehicle by itself. Otherwise choose the camera in the Camera tab while its vehicle is selected, and it comes back with that vehicle.

A camera is named by its address, such as `udp://127.0.0.1:5600`; **Rename** gives it a name of your own. Two streams to the same port show as two cameras, but giving each its own port is more reliable.

The Camera tab shows the selected vehicle's cameras: one large, the others small below it. Click a small one to show it large.

## An RTSP camera, or another port

In a Camera tab, choose **Add a camera** and give its RTSP address, with its user and password if it has them, or another UDP port. An RTSP camera can send H.264, H.265 or MPEG-TS. Give it a name, such as Front, to see that instead of its address. A vehicle's page lists its cameras and has **Add camera** too.

A port's camera shows straight away, waiting for video, so you can name it and choose its vehicle before anything is switched on. The first stream to arrive on the port becomes it. From a terminal:

```
uxduck video add rtsp://admin:password@192.168.144.25/main
uxduck video add udp:5601 --name Front
uxduck video remove udp:5601
```

A camera with more than one video track, such as a gimbal's day and thermal cameras, plays its first. Camera settings says how many more it offers and lets you choose; from a terminal, add `#track=2` to the address. `uxduck video list` shows each camera's tracks.

H.264 plays everywhere. H.265 plays in Chrome, Edge, Firefox and Safari where the computer's graphics can decode it: Macs, most Windows computers (Edge and Firefox may also need the HEVC Video Extensions) and Linux with VA-API.

UXDuck listens on 5600 by default, as QGroundControl does, so only one of them can have the port. Use another port, or none:

```
uxduck daemon --video-port 5601
uxduck daemon --video-port off
```

## In a window of its own

In UXDuck Desktop, **Open in new window** on a camera, or **Open Front in a new window** in the command palette, shows it in a window you can move to another screen. The window has the Camera tab's choices: one camera, its vehicle's cameras in a grid, or **Every camera**, from every vehicle. Press **F** to fill the screen with it. While it is open, the Camera tab says the camera is showing in another window, with **Show window** and **Show here**. Closing the main window closes its video windows.

Keyboard control keeps flying while you click in a video window, and Esc there turns it off. Switching to another app turns it off, and UXDuck says so.

## What the camera sees

A camera that sends KLV (MISB ST 0601) draws what it sees on the map under its vehicle, and Telemetry lists where it is and where it looks. The KLV can come inside MPEG-TS or, from an RTSP camera, in a track of its own beside the video; if the camera refuses that track, its video still plays and Camera settings says so.

## The camera's own controls

A camera that speaks the MAVLink camera protocol has its own controls in the Camera tab, above its picture, or at the top of the tab when its video does not arrive here. Only what the camera says it can do is shown:

- **Photo mode** and **Video mode**.
- **Photo to card**, and photos at an interval: give the seconds between them, press **Start interval**, and later **Stop interval**.
- **Record to card** and **Stop card recording**, with the camera's own count of the recording.
- **Zoom** and **Focus**, a step at a time, and **Autofocus**.
- The card: how many photos it holds and the room left on it. The line follows the card as it fills, is formatted or is swapped. Open it to see the photos the camera took while UXDuck listened, each with where it was taken, and to **Reset settings** or hold **Format card**. Formatting erases every photo and video on the card.

Each control says what the camera answered: waiting, working on it, accepted, or refused and why. A full card refuses photos and recordings until it is formatted or changed. A camera that only sends video has no controls, and neither has one that has not yet said what it is.

The list of photos shows the newest 20 and says how many there are, **Show 50 earlier** reads the ones before them, and **Show all** reads every one. UXDuck keeps the list until it stops, and formatting the card empties it.

UXDuck keeps the newest 100,000 photos a camera describes, because the camera sets how many it describes and UXDuck holds them in memory. A camera taking a photo every second reaches that after 28 hours of taking them. When a camera describes more, the oldest leave the list, and the list says how many older photos are not listed. The photos themselves stay on the camera's card.

A camera describes each photo as it takes it. When a camera counts a photo and does not describe it, UXDuck asks for it, however many are missing, a few at a time, and the list says how many it is still asking for. A photo asked for three times without an answer is counted as lost. A camera that describes none of 64 photos in a row does not answer such requests, so UXDuck stops asking it. The list then says how many photos were asked for and lost, and how many were not asked for. UXDuck asks again once the card is formatted, or once the camera has gone away and come back. A camera that is not heard itself is not asked for photos until it is heard again, so a request it could not hear does not count against it.

These photos and recordings stay on the camera's card. **Take a photo** and **Record** in the toolbar keep the picture on this computer, and with **Also save on the vehicle** they send the camera the same commands. A camera in photo mode that says it records only in video mode is not sent the recording command, because it would refuse it. The Camera tab says the card is not recording, and UXDuck does not switch the camera's mode for you: press **Video mode**, then **Record to card**.

Agents and other programs use the same commands:

```
uxduck rpc call camera/list
uxduck rpc call camera/command --json '{"cameraId":"CAMERA_ID","action":{"type":"take-photo"}}'
uxduck rpc call camera/images --json '{"cameraId":"CAMERA_ID","before":25,"limit":20}'
```

`camera/list` carries each camera's newest 20 photos, and `photosListed` says how many there are. `camera/images` reads the ones before them: `before` is the `index` of the first photo already read, and `earlier` in the answer says how many come before the page. When older photos are not listed, `photosDropped` in `camera/list` and `dropped` in `camera/images` say how many, and `photosNotListed` and `notListed` say it in words.

## The camera's own settings

Some cameras describe their own settings in a file, a camera definition, and name it when they say what they are. UXDuck reads the file and lists those settings under **Its own settings** in Camera settings: exposure, ISO, a video resolution, or whatever else the camera's maker put there. A new camera needs no new version of UXDuck.

- A setting with a list of values is a menu, and one with a range takes a number. A setting the camera sets itself is shown and cannot be changed.
- A choice can rule others out. With exposure on automatic, a camera may take no ISO. Such a setting stays in the list without its control and names the setting that rules it out.
- Each change says what the camera answered: waiting, accepted, or refused and why. Once the camera accepts, UXDuck reads the value back from it, and reads again the settings the file says may change with it.
- **Read again** reads every value from the camera.
- A setting the file gives no control of its own, such as the camera's mode, is left out, because the Camera tab already has it.
- UXDuck reads at most 1,024 names from one camera's list of settings, because the list is the camera's own to send. When a camera lists more, Camera settings says how many were read and how many were not, and a setting that shows no value may be one that was not read.
- Text in the file, such as a setting's description, is kept to 256 characters. Longer text is shown cut and ends in …, and Camera settings says how many texts were cut.

A camera usually serves the file itself over MAVLink FTP, and UXDuck fetches that without asking. A camera can name an `http` or `https` address instead. UXDuck Desktop then shows the address and fetches the file only when you press **Fetch the file**. UXDuck Web does not fetch from an address and says so. The file is read as a list of settings and nothing in it is run. UXDuck keeps the file with its version and fetches it again only when the camera names another version.

A file that is missing, larger than 1 MiB or not a camera definition is not used. Camera settings says why, and the camera's picture and its other controls work as before.

A camera its autopilot speaks for has no settings of its own here, because they are read from a camera that is a MAVLink component.

Agents and other programs read and change the same settings:

```
uxduck rpc call camera/settings --json '{"cameraId":"CAMERA_ID"}'
uxduck rpc call camera/command --json '{"cameraId":"CAMERA_ID","action":{"type":"set-setting","name":"CAM_ISO","value":800}}'
```

## A thermal picture, and tracking

A camera with more than one sensor has **Picture from** among its controls, with **Daylight** and **Thermal**. The pressed one is the picture the camera says its stream shows, so it changes when the camera has changed and not when you press. While the thermal picture shows, a camera that reports its range has it beside the buttons, such as "Thermal range 12.0 to 38.5 °C", and a camera that says where its warmest and coldest points are has them marked on the picture. UXDuck shows what the camera reports and measures nothing itself.

A camera that tracks has **Track**. Press it, then click a point in the picture, or drag a box round what to track. Enter tracks the middle of the picture, and Escape cancels. A camera that tracks only points takes the middle of a box, and one that tracks only boxes takes a small box round a click. The picture then has a ring or a box where the camera says its target is, which stays on the target as the vehicle and the gimbal move. The controls say "Tracking a point" or "Tracking a box", and **Stop tracking** ends it. When the camera also reports where its target is on the ground, the map and the 3D map mark the place with a red ring.

- "Target lost. The camera cannot find it in its picture." means the camera still tracks and cannot see its target. The ring goes until the camera finds it again.
- "The camera has not said what it tracks for 5 s." means the camera has stopped reporting. The ring stays where the target last was, dashed and dimmed.
- A camera that refuses to track says why, as its other controls do.

Without **Track** pressed, a drag on the picture turns the gimbal as before. Whether the gimbal follows the target is the camera's choice. Tracking is the camera's own: UXDuck asks, shows what the camera answers, and never steers the vehicle to a target.

A camera that says nothing of a second sensor is offered no choice, and one whose flags name no tracking has no **Track**.

Agents and other programs use the same commands. `camera/list` has each camera's `sources` and whether its stream is `thermal`, and the vehicle's telemetry has what the camera tracks (`camera.tracking`, `camera.tracking_x`, `camera.target_latitude_deg` and the rest) and its range (`camera.thermal_min_degc`, `camera.thermal_max_degc`). A point and a box are given as shares of the picture, from 0 at the left and the top to 1 at the right and the bottom.

```
uxduck rpc call camera/command --json '{"cameraId":"CAMERA_ID","action":{"type":"set-source","source":"thermal"}}'
uxduck rpc call camera/command --json '{"cameraId":"CAMERA_ID","action":{"type":"track-point","x":0.5,"y":0.75}}'
uxduck rpc call camera/command --json '{"cameraId":"CAMERA_ID","action":{"type":"track-rectangle","left":0.25,"top":0.5,"right":0.5,"bottom":0.75}}'
uxduck rpc call camera/command --json '{"cameraId":"CAMERA_ID","action":{"type":"stop-tracking"}}'
```

## The gimbal

A vehicle with a gimbal that speaks the MAVLink gimbal protocol has a row for it at the top of the Camera tab. UXDuck finds the gimbal by asking the autopilot and every component for one, so there is nothing to set up. The row says who is in control of it (UXDuck, no one, the autopilot, the mission, or another MAVLink system and component) and where it points: its pitch, and its yaw from the vehicle's nose or from north.

- **Drag on the picture** to turn the gimbal. The picture is pulled with your hand, so dragging right turns the gimbal left and dragging down tilts it up. While you drag, the picture says where it will point, and the gimbal is told once when you let go.
- **Centre** points it level and straight ahead.
- **Follow nose** lets its yaw turn with the vehicle. **Hold north** keeps its compass heading while the vehicle turns.
- **Pitch**, **Yaw** and **Point** send an angle you type. Pitch is from the horizon with down negative. Yaw is from the nose with right positive, or from north while the gimbal holds north.
- **Look here** in the map's menu, on a right-click or a long press, keeps the gimbal looking at that place on the ground. It is there for a gimbal whose manager says it can look at a place. PX4's does not say so.

UXDuck takes control of the gimbal by itself before it turns it, and gives control back about 15 seconds after the last movement. **Take control** keeps it until you press **Give back control**.

When the autopilot, a mission's gimbal step or another station has the gimbal, turning it would take it from them, so UXDuck asks first: **Take the gimbal** or **Leave it**. An ArduPilot autopilot does not say when a mission has the gimbal, so UXDuck works it out from the mission being flown.

While a Camera tab or the 3D map's Drone camera shows the gimbal, UXDuck asks it to say where it points ten times a second, and once a second otherwise.

A gimbal can stop talking while its autopilot goes on answering for it. Its row then says when the gimbal itself was last heard, and that a command may be accepted and move nothing. The angles beside it are marked as possibly not where it points now, and the Cockpit's Controls and **Look here** say the same. The buttons stay, because the autopilot still takes the commands. When the gimbal is heard again the line goes.

### With PX4

A PX4 autopilot that manages a gimbal (`MNT_MODE_IN` 4) turns it only for whoever is in control, so UXDuck takes control in its own name before each movement. PX4 says who is in control once every two seconds, so the row can be that far behind. If UXDuck's link is lost while a controller turns the gimbal, PX4 stops it about two seconds later. PX4 takes the gimbal itself when the vehicle returns or lands and when a mission ends in the air. It centres the gimbal and then leaves it with no one. PX4 turns the gimbal for a mission only once the mission is in control of it, so UXDuck uploads a **Set gimbal control** command ahead of each **Point gimbal** step of a plan. The vehicle then holds one more item than the plan for each such step, and the upload says how many were added. At each of those steps the mission takes the gimbal from whoever has it, UXDuck and a companion computer included. Press **Take control** and the gimbal is yours until the plan's next Point gimbal step. A mission that is resumed points the gimbal again as its last Point gimbal step did. After a mission that lands with its own Land step, PX4 leaves the gimbal where the last step pointed it, with the autopilot in control, until you press Take control.

When a gimbal reports that something is wrong with it, such as a motor, power or encoder fault, its controls say so: "The gimbal reports a motor fault. It may not point where it is asked." UXDuck still sends it your commands.

Agents and other programs use the same commands:

```
uxduck rpc call gimbal/list
uxduck rpc call gimbal/command --json '{"gimbalId":"GIMBAL_ID","action":{"type":"point","pitchDeg":-30,"yawDeg":20}}'
uxduck rpc call gimbal/command --json '{"gimbalId":"GIMBAL_ID","action":{"type":"look-at","latitudeDeg":47.3977,"longitudeDeg":8.5456}}'
```

The other actions are `centre`, `follow`, `hold-north`, `take-control` and `release-control`. A command that takes the gimbal from the autopilot, a mission or another station is refused until it is sent again with `"confirm":true`. An agent's is put to you first.

### With a controller or the keyboard

A controller or the keyboard that flies the vehicle turns its gimbal too. If the vehicle has more than one gimbal, they turn the first.

- **A gamepad's direction pad** tilts and turns the gimbal at 30 degrees a second while it is held. It is set in the built-in Mode 2 and Mode 1 layouts and in a mapping made from them. A mapping you made before this version keeps its buttons as they were.
- **Any button** can be held to tilt or turn the gimbal. In a mapping's editor, choose one of the four under **Gimbal, while held** for the button.
- **An input that springs back to its centre**, such as a trigger or a spare stick on a radio, can be given to **Gimbal pitch** or **Gimbal yaw** under **Other inputs**. The further it is pushed the faster the gimbal turns, up to 30 degrees a second, and **Reversed** turns it the other way. Each of the two takes one input.
- **The keyboard**, while keyboard control is on: **I** and **K** tilt the gimbal up and down, and **J** and **L** turn it left and right, at 30 degrees a second.

The gimbal stops when you let go. It also stops when the controller is unplugged or released, when keyboard control ends or its window stops sending keys, and when UXDuck is closed. The first push takes control as a drag does, and UXDuck gives control back about 15 seconds after you let go.

A controller is never asked a question, so it takes the gimbal from no one. While the autopilot, a mission or another station has the gimbal, nothing is sent. The Cockpit's Controls, the Camera tab and a notification say so. Press **Take control** there, answer **Take the gimbal**, and push again.

If the link to the vehicle is lost while you are pushing, UXDuck cannot tell the gimbal to stop. ArduPilot stops turning it about 3 seconds after the last rate it heard. Another gimbal manager may keep turning until the link returns, when UXDuck sends the stop.

## Recording

UXDuck records each camera while its vehicle is armed. Change this in **Settings › Video**, or choose **Record** in a Camera tab to start now. Recordings are listed in the Camera tab and play there. They are kept in your Movies folder, in **UXDuck** (Videos on Windows and Linux); **Show in Finder** opens it.

```
uxduck video recordings
```

## A simulation's camera

In UXDuck Desktop, a simulation started with **Camera** ticked has a camera in the Camera tab like any other, and one started with **Gimbal** ticked has a gimbal that its autopilot manages. Both are offered with ArduPilot and with PX4. Its picture is the 3D map's view from the vehicle: where the gimbal points, and as far as the camera has zoomed. It records, takes photos and opens in a window of its own as a real camera's video does. It serves a definition file of its own, so **Its own settings** in Camera settings can be tried with no hardware. With no map key the picture is a plain globe with a grid on it.

A UXDuck window on the computer that runs the simulation draws the picture, so one has to be open there. UXDuck Desktop keeps drawing while its window is minimised or covered. A browser tab on that computer draws only while it is shown. When nothing draws the picture, the Camera tab says why. The picture is drawn only while someone watches the camera or it is being recorded.

### Other kinds, and faults

Under **Advanced** in New simulation, **Kind of camera and gimbal** gives an ArduPilot simulation another kind than the full camera with a separate gimbal: a video-only camera, a camera that takes photos in photo mode and records in video mode, a camera and gimbal in one unit that manages its own gimbal, or a thermal and tracking camera with its gimbal. A video-only camera has no settings of its own. Snapshots and **Start again** keep the kind. A PX4 simulation carries the full camera and the separate gimbal only.

The thermal and tracking camera is one unit too, with a second, thermal picture and tracking. Its thermal picture is the daylight one in false colour, so it shows what is bright and not what is warm, and the range it reports is a fixed scale from 0 to 100 °C that measures nothing. It tracks a place on the ground, which it takes as flat and at home's height, and turns its own gimbal to keep the place in the middle of its picture. Point the gimbal yourself and it stops following, and goes on tracking until you press **Stop tracking**.

The simulation's page has **Simulated faults**, with a switch for each thing that can go wrong: the camera or the gimbal stops talking, the card is full, a command you choose is refused or takes five seconds, the gimbal's motor fails, or the autopilot holds the gimbal. The Camera tab shows each as it would for real hardware with that fault. A fault lasts until you switch it off or the simulation stops, and a full card ends when you format it. A PX4 simulation has no faults to switch yet.

```
uxduck sim start --autopilot px4 --vehicle multicopter --camera --gimbal
uxduck sim start --camera --gimbal --payload-profile integrated
uxduck sim faults LINK_ID
uxduck sim fault LINK_ID card-full on
uxduck sim fault LINK_ID command-refused on --command 2000
```

The kinds are `stream-only`, `photo-video`, `integrated` and `thermal-tracking`. `sim faults` lists the faults a simulation can have, why it cannot have the others, and the commands one can name.

## When it does not play

```
uxduck video list
uxduck video probe --port 5600
```

`probe` says what arrives, and why it does or does not play.
