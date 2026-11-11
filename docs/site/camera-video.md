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

## Recording

UXDuck records each camera while its vehicle is armed. Change this in **Settings › Video**, or choose **Record** in a Camera tab to start now. Recordings are listed in the Camera tab and play there. They are kept in your Movies folder, in **UXDuck** (Videos on Windows and Linux); **Show in Finder** opens it.

```
uxduck video recordings
```

## When it does not play

```
uxduck video list
uxduck video probe --port 5600
```

`probe` says what arrives, and why it does or does not play.
