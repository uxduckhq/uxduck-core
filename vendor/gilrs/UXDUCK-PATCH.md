# Exact macOS attachment identity

Published `gilrs` 0.11.2 and `gilrs-core` 0.6.8 from crates.io, upstream
commit `07e286e24b046cf39e5c367daa2770b805a64692` in
<https://gitlab.com/gilrs-project/gilrs>. Their `.cargo_vcs_info.json`,
original manifests, source, examples, SDL mappings and mapping license are
retained. Upstream's root LICENSE-MIT and LICENSE-APACHE at that commit are
included in both directories; the published crates omitted these files.
Cargo cache markers and the crates' independent lockfiles are omitted.

The accessor commit exposes the macOS backend's existing `entry_id` as
`MacOSGamepadExt::registry_entry_id() -> Option<u64>` through both crates,
following `LinuxGamepadExt`'s forwarding pattern. Disconnected gamepads return
None. This is an inert IORegistry attachment identifier, not a pointer or
persistent identity. Input processing, mapping GUIDs, event queues and all
other platforms are unchanged by that commit.

A separate backend repair changes only macOS removal bookkeeping: look up
the departing device's registry entry ID, never its shared LocationID, and
mark that DeviceInfo disconnected in the callback before queuing the usual
Disconnected event. This prevents duplicate/queued removals from selecting
the wrong slot. The consumer still delivers the same event and clears its
Gamepad state. Four pure tests cover identical locations, queued removals,
replacement before consumption, and per-slot accessor invalidation.

UXDuck joins this ID to the current IOHID attachment before deriving a
persistent unit identity from its serial number. A model GUID or singleton
VID/PID match cannot establish that join when disconnect events are queued.
Remove these two patches once the pinned upstream versions expose an
equivalent exact-attachment accessor.

# Resting inputs on Linux (FB-154)

The Linux evdev backend reported an axis only once it changed, so a stick
resting at centre had no value until moved. A radio set up and calibrated
earlier came back after a restart or a replug with its centred sticks
unread, and UXDuck asked for its setup again.

`Gamepad::open` now hands the opened device to `Gamepad::ready`, which
reads every axis's current `EVIOCGABS` value and queues it. `Gamepad::event`
delivers these first, as a new `gilrs_core::EventType::AxisValueOnConnection`.
`Gilrs::found` (start) and `Gilrs::plug` (hotplug) check each newly opened
pad once, so the values arrive before the device's next report.

Left out of the queue: an axis whose value cannot be read (never reported
as centred), an axis whose range is empty (gilrs would scale it to NaN),
and the hats `ABS_HAT0X` to `ABS_HAT3Y`, which gilrs reads as D-pad
presses. Buttons are never read on connection. In `gilrs`, an
`AxisValueOnConnection` for an axis the mapping makes a button (a trigger,
for example) becomes `EventType::Dropped` and leaves the state untouched.
So nothing held while plugging in reads as a fresh press. Any other axis
is scaled and named as an ordinary axis change, so it agrees with later
ones.

Tests in `gilrs-core/src/platform/linux/gamepad.rs` cover the queue, the
left-out axes, `Gamepad::ready`, and delivery through `Gilrs::found` and
`Gilrs::plug` into a new and a reused slot. Remove this patch once
upstream reports the initial state of a Linux device.
