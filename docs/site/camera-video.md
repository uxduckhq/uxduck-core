---
title: Camera video
order: 7
group: Use your vehicle
summary: Receive camera video, resolve port conflicts and find recordings.
---

Watch and record your vehicle's camera beside the map, and see on the map where it's looking.

## Send the stream to UXDuck

1. Set the camera to send to this computer's address on UDP port **5600**: MPEG-TS, plain or in RTP, or H.264 or H.265 in RTP as QGroundControl expects.
2. A notification says **Video from …**. When UXDuck knows whose camera it is, a Camera tab opens beside the map with it; otherwise choose **Open video**.

A camera that the autopilot announces shows up under its vehicle by itself. Otherwise choose the camera in the Camera tab while its vehicle is selected, and it comes back with that vehicle.

A camera is named by its address, such as `udp://127.0.0.1:5600`; **Rename** gives it a name of your own. Two streams to the same port show as two cameras, but giving each its own port is more reliable.

The Camera tab shows the selected vehicle's cameras: one large, the others small below it. Click a small one to show it large.

## An RTSP camera, or another port

In a Camera tab, choose **Add a camera** and give its RTSP address, with its user and password if it has them, or another UDP port. An RTSP camera can send H.264, H.265 or MPEG-TS. From a terminal:

```
uxduck video add rtsp://admin:password@192.168.144.25/main
uxduck video add udp:5601
uxduck video remove udp:5601
```

A camera with more than one video track, such as a gimbal's day and thermal cameras, plays its first. Camera settings says how many more it offers and lets you choose; from a terminal, add `#track=2` to the address. `uxduck video list` shows each camera's tracks.

H.264 plays everywhere. H.265 plays on a Mac; on Windows it needs the HEVC Video Extensions.

UXDuck listens on 5600 by default, as QGroundControl does, so only one of them can have the port. Use another port, or none:

```
uxduck daemon --video-port 5601
uxduck daemon --video-port off
```

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
