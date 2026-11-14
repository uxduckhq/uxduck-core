"""UXDuck's camera watchdog (ADR-0060 C9), installed by UXDuck on request.

MediaMTX v1.21 keeps a Raspberry Pi camera's path ready after its camera
helper dies, with no pictures, and never restarts it. This polls MediaMTX's
local API every 2 s. A rpiCamera path MediaMTX reports ready that receives
no bytes for 10 s restarts the MediaMTX unit, which pauses every camera on this
computer for a few seconds. It acts armed or disarmed. It restarts at most
three times in ten minutes, then stops restarting and says so in the journal
and its state file, until it is itself restarted. An on-demand path nobody
reads is not judged, and neither is a path MediaMTX already reports not
ready, which MediaMTX retries itself.

Remove it from the computer's Cameras section in UXDuck, or by hand with
systemctl disable --now uxduck-camera-watchdog, then delete this file and
/etc/systemd/system/uxduck-camera-watchdog.service.
"""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time
import urllib.request

POLL_S = 2.0
FROZEN_S = 10.0
CAP = 3
WINDOW_S = 600.0
KEPT = 10
STATE = pathlib.Path("/run/uxduck/camera-watchdog.json")
# Built once: building an opener loads the system's certificates. Never through a proxy.
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def log(message):
    print(message, file=sys.stderr, flush=True)


class Watchdog:
    def __init__(self, unit, api, state=STATE):
        self.unit, self.api, self.state = unit, api, state
        # Each judged path's received bytes, and since when they have been that.
        self.seen = {}
        # (monotonic, wall clock, path) of each restart, newest last.
        self.restarts = []
        self.gave_up = False

    def fetch(self, endpoint):
        with OPENER.open("http://" + self.api + endpoint + "?itemsPerPage=1000", timeout=3) as response:
            return json.load(response).get("items") or []

    def restart(self):
        subprocess.run(["systemctl", "restart", "--", self.unit], check=True, capture_output=True, timeout=120)

    def step(self, now, wall):
        """One poll: now is a monotonic clock, wall the time of day."""
        try:
            configured = self.fetch("/v3/config/paths/list")
            live = {item.get("name"): item for item in self.fetch("/v3/paths/list")}
        except (OSError, ValueError):
            # MediaMTX is down or restarting: nothing to judge until it answers.
            self.seen.clear()
            self.save(wall, [])
            return
        judged, frozen = [], None
        for path in configured:
            name, item = path.get("name"), live.get(path.get("name"))
            if path.get("source") != "rpiCamera" or item is None or not item.get("ready", item.get("available")):
                self.seen.pop(name, None)
                continue
            if path.get("sourceOnDemand") and not item.get("readers"):
                self.seen.pop(name, None)
                continue
            judged.append(name)
            received = item.get("inboundBytes", item.get("bytesReceived", 0))
            last = self.seen.get(name)
            if last is None or received != last[0]:
                self.seen[name] = (received, now)
            elif frozen is None and now - last[1] >= FROZEN_S:
                frozen = name
        if frozen:
            self.act(frozen, now, wall)
        self.save(wall, judged)

    def act(self, name, now, wall):
        if self.gave_up:
            return
        if len([r for r in self.restarts if now - r[0] < WINDOW_S]) >= CAP:
            self.gave_up = True
            log("Camera " + name + " has received nothing for 10 s again, after " + str(CAP) + " restarts of " + self.unit +
                " in ten minutes. The watchdog stops restarting it. Restart " + self.unit + " by hand, then this watchdog to let it act again.")
            return
        log("Camera " + name + " has received nothing for 10 s. Restarting " + self.unit + ", which pauses every camera on this computer for a few seconds.")
        self.restarts = (self.restarts + [(now, wall, name)])[-KEPT:]
        self.seen.clear()
        try:
            self.restart()
        except (OSError, subprocess.SubprocessError) as error:
            log("Could not restart " + self.unit + ": " + str(error))

    def save(self, wall, judged):
        state = {"unit": self.unit, "updatedAt": wall, "judged": judged, "gaveUp": self.gave_up,
                 "restarts": [{"at": at, "path": path} for _, at, path in self.restarts]}
        try:
            self.state.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
            fd, name = tempfile.mkstemp(prefix=".camera-watchdog-", dir=self.state.parent)
            with os.fdopen(fd, "w") as file:
                json.dump(state, file)
            os.chmod(name, 0o644)
            os.replace(name, self.state)
        except OSError as error:
            log("Could not write " + str(self.state) + ": " + str(error))


def main():
    parser = argparse.ArgumentParser(description="UXDuck's camera watchdog")
    parser.add_argument("--unit", required=True)
    parser.add_argument("--api", required=True)
    args = parser.parse_args()
    watchdog = Watchdog(args.unit, args.api)
    log("Watching the rpiCamera paths of " + args.unit + " through http://" + args.api + ".")
    while True:
        watchdog.step(time.monotonic(), time.time())
        time.sleep(POLL_S)


if __name__ == "__main__":
    main()
