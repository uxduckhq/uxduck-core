---
title: Watch links
search: Share a live drone flight with a read-only link
order: 13.5
group: Preview features
summary: Let anyone watch a flight in a browser, read only, with a link.
---

Send a link, and anyone who opens it watches your vehicle live in their browser: the map and its values with how old each is. They need no account and no UXDuck, and they can't command anything. For now, watch links share neither the camera nor your map markers.

## Share a link

You need UXDuck installed and signed in to your account, with **Settings › Account › Live sharing from this computer** on (`uxduck account live-sharing on` from a terminal).

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

UXDuck itself, watch only: the vehicle on the map with its trail, its mode, armed state, values with their age in Telemetry, who flies and how many are watching. Not the camera or your markers: video isn't shared through watch links for now. They can open the 3D map, move and resize the panels, and switch between the link's vehicles. They get nothing that needs UXDuck of their own: no AI, no settings, and nothing that commands the vehicle. If your computer drops, the page says **offline** and catches up when it's back. A link admits 100 people at once. The page counts each visit anonymously, without the link's address.

## End a link

**Settings › Account › Watch links** lists each link with how many are watching; **Revoke** ends it at once: nothing more reaches anyone watching, and their page says **This watch link has ended**, up to about 10 seconds later. If UXDuck can't close everyone's page at that moment, it says so: choose **Revoke** again.

Turning off **Live sharing from this computer**, or signing out on this computer, ends every link of this computer. If UXDuck can't reach its servers at that moment, it shares nothing more from this computer and keeps trying until the links have ended. Signed out while offline, it can't end them: they run until they expire, until you sign in on this computer again, or until you sign this computer out from another device, with nothing new shared meanwhile.

A link ends with the sign-in that made it. Signing this computer out from another device, in **Settings › Account › Where you're signed in**, ends its links at once and closes everyone's page. If UXDuck can't close every page at that moment, it says so: sign it out again.
