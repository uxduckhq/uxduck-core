---
title: Watch links
order: 13.5
group: Preview features
summary: Let anyone watch a flight in a browser, read only, with a link.
---

Send a link, and anyone who opens it watches your vehicle live in their browser: the map, its values with how old each is, and the camera. They need no account and no UXDuck, and they can't command anything.

## Share a link

You need UXDuck installed and signed in to your account, with **Settings › Account › View this computer’s vehicles remotely** on.

1. Open the vehicle's menu (**…**) and choose **Share a watch link…**.
2. Choose what it shows (this vehicle, or every vehicle on this computer), who flies, as the page shows it, and how long it lasts: 24 hours unless you choose, a week at most.
3. Choose **Make the link**, then **Copy**, and send it. The address is shown only now.

From a terminal:

```
uxduck account watch-link make --vehicle Alpha --flown-by Ben --hours 2
uxduck account watch-link list
uxduck account watch-link revoke wl3f9a1c0d7e2b4a6c
```

## What people see

The vehicle on the map with its trail, its mode and armed state, its values with their age, its notices, the camera when they open it, and how many are watching. If your computer drops, the page says **offline** and catches up when it's back. A link admits 100 people at once.

## End a link

**Settings › Account › Watch links** lists each link with how many are watching; **Revoke** ends it at once, and everyone watching sees **This watch link has ended**. Turning off **View this computer’s vehicles remotely**, or signing out, ends every link of this computer.
