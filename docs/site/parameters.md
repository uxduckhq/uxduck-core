---
title: Parameters
search: ArduPilot parameters: compare, edit and stage changes
order: 14
group: Use your vehicle
summary: Compare and stage ArduPilot parameter changes, with descriptions from the matching Plane, Copter, Rover or Sub firmware.
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

## Saved parameters

**Save these…** keeps every value as it is now under a name, such as "Field tune", as a `.param` file in the vehicle's Files on this computer. **Saved parameters** lists them newest first, with how many values differ from the vehicle now. Open one to compare it, then hold **Apply** to write every value that differs: the values before are saved first as "Before Field tune", so switching back is one more apply. **Import .param…** keeps a file as a saved set and compares it before anything is written. **Export .param** saves the vehicle's values in Mission Planner's `NAME,value` format. From the command line: `uxduck params sets`, `save`, `apply` and `export`.

## Restart

Some values take effect only once the vehicle restarts. UXDuck says so where you wrote them; **Restart…** opens the Restart row in the vehicle's setup, which lists what is waiting and restarts it with a hold.

## Your own descriptions

In **Settings › Parameters**, add the descriptions of your own firmware from a file, or from the URL your CI publishes them at; a set from a URL goes to your other computers with sync. From the command line: `uxduck params definitions add <file or URL>`.

With the ArduPilot source checkout that built your firmware, generate and add its descriptions:

```sh
uxduck params definitions generate path/to/ardupilot --vehicle Plane
```

Use `Copter`, `Plane`, `Rover`, `Sub`, `Tracker` or `Blimp` for `--vehicle`. Python 3 with `lxml` is required; `PYTHON` can point to an existing virtual environment's interpreter. The generated set records the checkout's commit and is used only for firmware with that commit. Keep a clean checkout so the descriptions match its recorded source. Remove one with `uxduck params definitions remove <set-id>`.

PX4 parameter descriptions are not supported yet.

## Your fork's messages, and each vehicle's definitions

In **Settings › MAVLink messages**, add your fork's MAVLink XML and give the commit of each build that has it. A vehicle reports the commit its firmware was built from, so every vehicle running those builds decodes the fork's messages in the MAVLink inspector, for the agent and for plugins, without anyone choosing. A dialect that changes a standard message applies only to those vehicles, or to the vehicles it is chosen for.

To choose for one vehicle, open its setup's **Definitions**: turn dialects on or off, add one from its XML files, and pick ArduPilot's descriptions and your own separately. If your CI publishes a firmware profile, add its URL under **Firmware profiles** in the same settings, or with `uxduck firmware add <url>`: each build's parameters and messages arrive with its commit and are fetched again daily.

From the command line:

```sh
uxduck mavlink dialects add acme.xml --commit 2a3dc4b7 --commit 9f00aa12
uxduck mavlink dialects use Alpha --dialect "Acme fork"
```

With the ArduPilot checkout that built your firmware, add its messages for builds of its commit in one step: `uxduck mavlink dialects generate path/to/ardupilot` reads its `ardupilotmega.xml` and the files that includes. Only the messages your fork adds or changes are kept; a standard message that only lacks fields newer MAVLink added still decodes with UXDuck's own.

## Missing descriptions

Parameter values can download even when descriptions are unavailable. Check `uxduck params definitions list` for the catalogue status and the sets you have added. An agent should report `describedBy`, or say that no matching description is available, before interpreting a value.

A message about **no trusted public verification key** means this build cannot verify the publisher's parameter catalogue. You do not need to provide a secret or a release signing key. The publisher includes the public verification key in the build. Until a build with that key is available, add descriptions from the matching ArduPilot checkout using the command above.

For a development firmware, use the exact commit that built it, rather than a nearby release. PX4 descriptions remain unavailable; values alone do not establish their meaning. For an incomplete parameter download, check the link and message freshness in [Troubleshooting](/docs/troubleshooting).
