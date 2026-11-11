---
title: Confirmations
order: 10
group: Use your vehicle
summary: Which commands go on a click and which you press and hold.
---

UXDuck never stops you with a question. Commands that could hurt someone or the vehicle are pressed and held instead.

## Click or hold

- **Click:** modes, return to launch, land, take off, going to a place, and setting home or the EKF origin on the ground.
- **Press and hold:** arming and disarming, rebooting, and forcing arm or disarm, on the ground and in flight alike, and setting home or the EKF origin in flight. These buttons have a ring before their name that fills while you hold; they act when it is full. A quick click does nothing and says "Press and hold".

From a shortcut or the action list, a command you hold opens on its own with its button ready: hold **Space** until it fills, or press **Esc** to cancel.

When a vehicle refuses to arm or disarm, the Cockpit tab says why and shows **Force arm** or **Force disarm** to hold.

When a vehicle is offline, the Cockpit tab says so once. Commands still go to its last known address and may arrive late.

## Send it to a place

On the Cockpit tab, press **Go to** (or **G**), set the height, then click the map: the vehicle goes there. Each click sends it again, until you press **G**, **Esc** or **Stop**, or show another part of the Cockpit, such as **Modes**. You can also type a latitude and longitude and press Enter.

## Set home

Home is where the vehicle returns to and lands, and where its heights are measured from. On the Cockpit tab, open **Home** and press **Set home**, then click the map: home moves there, on the ground, at the height UXDuck's terrain gives. Press **Esc** or **Stop** when done. **Use vehicle position** moves home to where the vehicle is now, on the ground under it while it is armed, and **Enter coordinates** takes a latitude and longitude, with a height above sea level if you know it.

Under **Advanced**, **Set EKF origin** gives an ArduPilot vehicle without GPS the point its position estimate is measured from, in the same way. ArduPilot takes an EKF origin only while it has none, and a home only once it has an origin, so a vehicle without GPS needs its origin first.

From the command line: `uxduck command set-home --lat -35.3619 --lon 149.1652`, or with no position for where the vehicle is, and `uxduck command set-ekf-origin --lat … --lon …`.

## Approvals

When an agent asks to send a command, you approve it once. **Settings › Approvals** lists what you always allowed, each with **Reset**.
