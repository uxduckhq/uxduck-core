---
title: Connect a vehicle
order: 3
group: Get started
summary: Connect USB, radio, UDP or TCP and choose the right vehicle.
---

A link is how UXDuck reaches a vehicle: a USB cable, a telemetry radio or the network.

## Found by itself

Open **Connect a vehicle**. A flight controller on USB, a simulator on this computer or a vehicle's Wi-Fi shows up with **Connect**. Nothing connects until you press it. From a terminal:

```
uxduck find
uxduck find --all-serial
uxduck find --network
```

## USB or a telemetry radio

1. On **Home**, choose **USB or serial**.
2. Pick the **Device** (such as `/dev/ttyACM0` or `COM3`) and the **Baud rate** (115200 for USB, usually 57600 for a radio).
3. Choose **Add link**. The vehicle appears in the sidebar.

## Over the network

1. On **Home**, choose **Network**.
2. Pick how the vehicle reaches you:
   - **UDP listener** on port 15010: the vehicle sends to this computer.
   - **UDP client**: the vehicle is behind a UDP server, such as a Wi-Fi telemetry bridge or mavlink-router. Give its address, such as `192.168.4.1` and port 14550.
   - **TCP client**: a TCP server, such as SITL on port 5760.
   - **TCP listener**: the vehicle or a companion computer connects to this computer. Each connection is its own vehicle.
3. Choose **Add link**.

From a terminal instead:

```
uxduck rpc call link/add --json '{"kind":{"type":"udp-listen","host":"0.0.0.0","port":15010}}'
```

## Forward to another ground station

Add a **Forward to a program** link, such as Mission Planner on `127.0.0.1:14550`.

On a Raspberry Pi beside a radio, without a window:

```
uxduck forward --in serial:/dev/ttyAMA0@57600 --out udp:192.168.1.20:15010
```

`--in` also takes `udpout:HOST:PORT` (a UDP server), `tcp:HOST:PORT` and `tcpin:HOST:PORT` (listens for TCP).

## Change or turn off a link

On **Vehicles**, each link has a switch and a pencil. The switch turns a link off or on without removing it. The pencil opens the link's settings in place: change its port, device, access or name and choose **Save**. **Remove** is in the settings.

From a terminal:

```
uxduck rpc call link/update --json '{"id":"<link id>","kind":{"type":"udp-listen","host":"0.0.0.0","port":15011}}'
uxduck rpc call link/disable --json '{"id":"<link id>"}'
uxduck rpc call link/enable --json '{"id":"<link id>"}'
```

## When nobody is watching

UXDuck sends each vehicle a ground station heartbeat. If no UXDuck window is open and nobody is in control remotely, it stops after 10 s (**Settings › UXDuck on this computer**), and a vehicle whose lost ground station failsafe is on does what it is set to do, such as return to launch. Keep a window open while flying.
