---
title: Parameters
order: 14
group: Use your vehicle
summary: Compare and stage parameter changes; load matching firmware descriptions.
---

See, search and change a vehicle's parameters, with a history of every change.

## Download

A vehicle's parameters download when it is first heard and after each restart. A dot marks a value changed from its default, and **Changed from default** shows only those.

## Find one

Parameters group by name: `BATT_FS_LOW_ACT` sits under `BATT_`, then `BATT_FS_`. To group parameters your own way, open one and add it to a group, such as "Hover tuning"; your groups come first. They are kept in this browser.

## Change several at once

Choose a value, change it and choose **Stage**. Staged changes wait at the top of the tab: give a reason if you like, then **Write** them together, or **Discard** them. UXDuck writes each change only if the vehicle still has the value you started from. A change the vehicle refuses stays staged.

## History

Every change of value goes in the vehicle's **History** with who made it and why: you, the agent, another ground station, the vehicle itself, or a change found when the parameters downloaded again. Values the vehicle sets at each start, such as ground pressure and gyro offsets, are left out.

## .param files

**Export .param** saves every value in Mission Planner's `NAME,value` format. **Import .param…** compares a file with the vehicle first, showing each difference with the file's value, the vehicle's and the units; **Stage** puts them with your other changes.

## Your own descriptions

In **Settings › Parameters**, add the descriptions of your own firmware from a file, or from the URL your CI publishes them at; a set from a URL goes to your other computers with sync. From the command line: `uxduck params definitions add <file or URL>`.

With the ArduPilot source checkout that built your firmware, generate and add its descriptions:

```sh
uxduck params definitions generate path/to/ardupilot --vehicle Plane
```

Use `Copter`, `Plane`, `Rover`, `Sub`, `Tracker` or `Blimp` for `--vehicle`. Python 3 with `lxml` is required; `PYTHON` can point to an existing virtual environment's interpreter. The generated set records the checkout's commit and is used only for firmware with that commit. Keep a clean checkout so the descriptions match its recorded source. Remove one with `uxduck params definitions remove <set-id>`.

PX4 parameter descriptions are not supported yet.

## Missing descriptions

Parameter values can download even when descriptions are unavailable. Check `uxduck params definitions list` for the catalogue status and the sets you have added. An agent should report `describedBy`, or say that no matching description is available, before interpreting a value.

A message about **no trusted public verification key** means this build cannot verify the publisher's parameter catalogue. You do not need to provide a secret or a release signing key. The publisher includes the public verification key in the build. Until a build with that key is available, add descriptions from the matching ArduPilot checkout using the command above.

For a development firmware, use the exact commit that built it, rather than a nearby release. PX4 descriptions remain unavailable; values alone do not establish their meaning. For an incomplete parameter download, check the link and message freshness in [Troubleshooting](/docs/troubleshooting).
