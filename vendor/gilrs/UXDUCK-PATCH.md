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
