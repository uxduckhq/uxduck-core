---
title: Air traffic
search: ADS-B air traffic near your drone
order: 9.5
group: Use your vehicle
summary: See ADS-B aircraft near your vehicles, from a vehicle's receiver, a receiver on your network, or adsb.lol.
---

Open **Airspace**, beside Telemetry. It lists every source of traffic with its switch, then aircraft near your vehicles, nearest first. Traffic helps you see; it is never complete.

To try it without a receiver, tick **Simulated aircraft nearby** in New simulation. Five made-up aircraft, named SIM…, fly around that simulated vehicle.

## Traffic on the map

Traffic starts with saved limits of **10 km horizontally** and **1 km above or
below** your selected locations. The 3D map uses small neutral dots; hover or tap
for callsign and altitude details. You can choose to keep labels visible.

Open the map's **Traffic** dropdown or **Airspace → Traffic display and alerts**
to change either threshold or turn it off. Choose all vehicles, individual
vehicles, your device location, or named ground locations entered as coordinates
or picked on the map. While the settings are open, dashed rings show display
range and solid amber rings show alert range, with altitude bands in the legend.

**Nearby**, **All received**, and **Off** in Layers and On-screen display are
shortcuts to these same choices for both maps. Nearby restores the saved numeric
limits; All received disables both limits; Off hides the map traffic. There is
no additional fixed 3D cutoff. Airspace still lists received aircraft with reasons
when they are filtered from the map. An aircraft with an unknown altitude shows
on the 2D map and in Airspace, but the 3D map leaves it out unless it sets off an
alert; unavailable locations are explained rather than hiding everything.

Alerts are independently configurable, enabled by default within **2 km** and
**300 m above or below** every vehicle with a current position. Neither arming
nor disarming changes your filters, alert settings or refresh interval.

## A vehicle's ADS-B receiver

A vehicle with a receiver (ArduPilot `ADSB_TYPE` set) appears under its name as soon as it reports aircraft. Turn its switch on to use them. Each vehicle is off until you do.

## A receiver on your network

A Raspberry Pi with an RTL-SDR stick and its antenna outside:

1. Install readsb on the Pi:

   ```sh
   sudo bash -c "$(wget -O - https://raw.githubusercontent.com/wiedehopf/adsb-scripts/master/readsb-install.sh)"
   ```

2. In UXDuck Desktop, open Airspace, choose **Add receiver**, keep **SBS-1 over TCP** and enter the Pi's address, such as `raspberrypi.local`. UXDuck connects to port 30003.

For a Stratux, choose **GDL90 over UDP** and port `4000`, with this computer on the Stratux's Wi-Fi.

## A USB stick on this computer

Plug an RTL-SDR stick with an antenna into the computer UXDuck Desktop runs on.

1. Install readsb. On a Mac: `brew install readsb`.
2. In Airspace, choose **Add receiver** and **A USB receiver stick on this computer**. UXDuck starts readsb on the stick.

If readsb is missing or the stick is unplugged, its row says so.

## adsb.lol

Turn on **adsb.lol** in Airspace to see aircraft from adsb.lol's community network. This computer sends adsb.lol the area your vehicles are in, within adsb.lol's free 500 requests a day.
