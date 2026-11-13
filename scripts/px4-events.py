#!/usr/bin/env python3
"""PX4's health and arming check events, for UXDuck to word PX4's reasons.

PX4 says why it cannot arm through MAVLink events: an id and its arguments,
worded by the events catalogue its build writes to etc/extras. This keeps
the catalogue's arming_check and health groups and the enums they use, with
the descriptions' developer notes taken out.

    python3 scripts/px4-events.py <all_events.json.xz> <PX4 version>

writes crates/uxduck-mavlink/px4/events.json. PX4's licence (BSD-3-Clause)
is beside it.
"""

import json
import lzma
import re
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "crates/uxduck-mavlink/px4/events.json"
GROUPS = ("arming_check", "health")


def plain(text: str) -> str:
    text = re.sub(r'<profile name="dev">.*?</profile>', "", text, flags=re.S)
    text = re.sub(r"<param>(.*?)</param>", r"\1", text)
    text = re.sub(r"<[^>]+>", "", text)
    return re.sub(r"\s+", " ", text).strip()


def enum_name(namespace: str, name: str) -> str:
    if name.startswith(("uint", "int")) or name in ("float", "double", "bool") or not name.endswith("_t"):
        return name
    return name if "::" in name else f"{namespace}::{name}"


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    source, version = sys.argv[1], sys.argv[2]
    catalogue = json.loads(lzma.open(source).read())
    events = {}
    used = set()
    for component_id, component in catalogue["components"].items():
        namespace = component["namespace"]
        for group in GROUPS:
            for sub_id, event in component.get("event_groups", {}).get(group, {}).get("events", {}).items():
                arguments = [enum_name(namespace, a["type"]) for a in event.get("arguments", [])]
                used.update(t for t in arguments if "::" in t)
                events[str((int(component_id) << 24) | int(sub_id))] = {
                    "name": event["name"],
                    "group": group,
                    "message": event["message"],
                    "description": plain(event.get("description", "")),
                    "arguments": arguments,
                }
    if not events:
        sys.exit(f"{source} has no {' or '.join(GROUPS)} events")
    enums = {}
    for component in catalogue["components"].values():
        for name, enum in component.get("enums", {}).items():
            full = f"{component['namespace']}::{name}"
            if full not in used:
                continue
            enums[full] = {
                "type": enum["type"],
                "bitfield": bool(enum.get("is_bitfield")),
                "entries": {value: entry.get("description") or entry["name"] for value, entry in enum["entries"].items()},
            }
    missing = used - set(enums)
    if missing:
        sys.exit(f"{source} lacks enums {sorted(missing)}")
    out = {"source": f"PX4 {version}, etc/extras/all_events.json", "events": events, "enums": enums}
    OUT.write_text(json.dumps(out, indent=1, sort_keys=True, ensure_ascii=False) + "\n")
    print(f"{OUT}: {len(events)} events, {len(enums)} enums")


if __name__ == "__main__":
    main()
