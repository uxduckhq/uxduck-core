import http.server
import importlib.util
import json
import os
import pathlib
import pwd
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

HERE = pathlib.Path(__file__).parent
# The fake /proc and /sys trees uxduck companion's Rust reads too (ADR-0067 M2).
MICROPHONES = HERE.parents[3] / "tests/fixtures/microphones"
spec = importlib.util.spec_from_file_location("camera", HERE / "camera.py")
camera = importlib.util.module_from_spec(spec)
spec.loader.exec_module(camera)

# A stand-in for systemctl, systemd-run and journalctl: it records each call
# and answers from files the test writes.
FAKE = r'''#!PYTHON
import json, os, pathlib, subprocess, sys
fake = pathlib.Path(os.environ["FAKE_DIR"])
name = pathlib.Path(sys.argv[0]).name
with open(fake / "calls.jsonl", "a") as log:
    log.write(json.dumps([name] + sys.argv[1:]) + "\n")
args = sys.argv[1:]
if name == "systemd-run":
    while args and args[0].startswith("-"):
        args = args[2:] if args[0] == "--unit" else args[1:]
    sys.exit(subprocess.run(args).returncode)
if name == "journalctl":
    print((fake / "journal.txt").read_text() if (fake / "journal.txt").exists() else "", end="")
elif args[:1] == ["list-units"]:
    print((fake / "units.txt").read_text() if (fake / "units.txt").exists() else "", end="")
elif args[:1] == ["show"]:
    print((fake / "show.txt").read_text(), end="")
elif args[:1] == ["is-active"]:
    print("inactive")
    sys.exit(3)
elif args[:1] == ["restart"]:
    count = fake / "restarts"
    count.write_text(str(int(count.read_text()) + 1 if count.exists() else 1))
'''


class FakeApi:
    """MediaMTX's local API, answering from functions the test sets."""

    def __init__(self):
        self.configured = []
        self.live = lambda: []
        api = self

        class Handler(http.server.BaseHTTPRequestHandler):
            disable_nagle_algorithm = True

            def do_GET(self):
                if self.path.startswith("/v3/config/paths/list"):
                    items = api.configured
                elif self.path.startswith("/v3/paths/list"):
                    items = api.live()
                else:
                    self.send_error(404)
                    return
                body = json.dumps({"itemCount": len(items), "pageCount": 1, "items": items}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.address = "127.0.0.1:" + str(self.server.server_address[1])
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class Growing:
    """A camera whose received bytes grow on every read, or stay put."""

    def __init__(self, ready=True, growing=True):
        self.ready, self.growing, self.bytes = ready, growing, 1000

    def item(self, name):
        if self.growing:
            self.bytes += 5000
        return {"name": name, "ready": self.ready, "readers": [], "inboundBytes": self.bytes}


class CameraTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = pathlib.Path(temp.name)
        self.fake = self.root / "fake"
        bin_dir = self.fake / "bin"
        bin_dir.mkdir(parents=True)
        for name in ("systemctl", "systemd-run", "journalctl", "loginctl"):
            script = bin_dir / name
            script.write_text(FAKE.replace("PYTHON", sys.executable, 1))
            script.chmod(0o755)
        environment = patch.dict(os.environ, {"PATH": str(bin_dir) + os.pathsep + os.environ["PATH"], "FAKE_DIR": str(self.fake)})
        environment.start()
        self.addCleanup(environment.stop)
        for name, value in {"SYSTEMD": self.root / "systemd", "WORK": self.root / "work", "LINGER": self.root / "linger",
                            "LOGIND": self.root / "logind.conf", "LOGIND_DIRS": (self.root / "logind.conf.d",),
                            "LOGIN_DEFS": self.root / "login.defs", "SENSOR_NAMES": (self.root / "video4linux",),
                            "WATCHDOG_SCRIPT": self.root / "lib/camera-watchdog.py", "WATCHDOG_STATE": self.root / "run/camera-watchdog.json",
                            "COMPANION_TOML": self.root / "uxduck/companion.toml",
                            "ASOUND": self.root / "proc/asound", "SOUND_CLASS": self.root / "sys/class/sound",
                            "READY_WAIT_S": 1.0, "POLL_S": 0.02}.items():
            patcher = patch.object(camera, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.api = FakeApi()
        self.addCleanup(self.api.close)
        self.drop_in = self.root / "systemd/mediamtx.service.d/60-uxduck-camera.conf"
        self.low = {"width": 1536, "height": 864, "fps": 50, "mode": "2304:1296:10:P", "idrPeriod": 50, "bitrate": 2_000_000}

    def calls(self):
        path = self.fake / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def found(self, **paths):
        return {"unit": "mediamtx.service", "api": self.api.address, "paths": [
            {"name": name, "source": source, "onDemand": on_demand, "sensor": "imx708"} for name, (source, on_demand) in paths.items()]}

    def plan(self, values):
        return {"unit": "mediamtx.service", "dropIn": str(self.drop_in), "content": camera.render(values),
                "api": self.api.address, "target": "cam", "paths": ["cam", "rear"]}

    def companion_toml(self, text):
        path = self.root / "uxduck/companion.toml"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(0o640)
        return path

    def test_a_cameras_recording_on_the_vehicle_is_read_with_it_and_its_recorded_path_leaves_the_list(self):
        # ADR-0066 R10: a [cameras.<path>] table names the recorded path and the mode.
        self.companion_toml('# Written by UXDuck\n[features]\ncameras = true\n\n[cameras.front]\nrecord_path = "front_record"  # 720p30\n'
                            '\n[cameras."rear"]\nrecord_path = "rear_record"\nrecord = "always"\n\n[cameras.side]\nrecord = "off"\n')
        record = {"width": 1280, "height": 720, "fps": 30, "mode": "", "idrPeriod": 30, "bitrate": 4_000_000}
        status = {"paths": [{"name": "front", "source": "rpiCamera"}, {"name": "front_record", "source": "rpiCamera", "settings": record},
                            {"name": "rear", "source": "rpiCamera"}, {"name": "rear_record", "source": "rpiCamera"},
                            {"name": "side", "source": "rpiCamera"}]}
        camera.recordings(status)
        self.assertEqual(status["paths"], [
            {"name": "front", "source": "rpiCamera", "record": {"path": "front_record", "mode": "armed", "settings": record}},
            {"name": "rear", "source": "rpiCamera", "record": {"path": "rear_record", "mode": "always"}},
            {"name": "side", "source": "rpiCamera"}])
        self.assertNotIn("recordProblem", status)

    def test_a_record_mode_uxduck_does_not_know_is_said_and_the_rest_is_read(self):
        self.companion_toml('[cameras.front]\nrecord_path = "front_record"\nrecord = "sometimes"\n')
        status = {"paths": [{"name": "front", "source": "rpiCamera"}, {"name": "front_record", "source": "rpiCamera"}]}
        camera.recordings(status)
        self.assertIn('record = "sometimes"', status["recordProblem"])
        self.assertEqual([p["name"] for p in status["paths"]], ["front", "front_record"])
        empty = {"paths": [{"name": "front", "source": "rpiCamera"}]}
        (self.root / "uxduck/companion.toml").unlink()
        camera.recordings(empty)
        self.assertEqual(empty, {"paths": [{"name": "front", "source": "rpiCamera"}]})

    def test_a_record_mode_is_written_on_its_own_line_and_every_other_line_is_kept(self):
        text = '# Written by UXDuck\n[cameras.front]\nrecord_path = "front_record"\n\n[cameras.rear]  # the back one\nrecord_path = "rear_record"\nrecord = "armed" # default\n'
        path = self.companion_toml(text)
        found = {"paths": [{"name": "front", "record": {"path": "front_record", "mode": "armed"}},
                           {"name": "rear", "record": {"path": "rear_record", "mode": "armed"}}, {"name": "side"}]}
        camera.set_record({"path": "front", "mode": "always"}, found)
        camera.set_record({"path": "rear", "mode": "off"}, found)
        self.assertEqual(path.read_text(), '# Written by UXDuck\n[cameras.front]\nrecord = "always"\nrecord_path = "front_record"\n\n'
                                           '[cameras.rear]  # the back one\nrecord_path = "rear_record"\nrecord = "off"\n')
        self.assertEqual(path.stat().st_mode & 0o777, 0o640)
        # Nothing runs while uxduck companion is not running: it reads the file when it starts.
        self.assertEqual([c for c in self.calls() if c[:2] == ["systemctl", "reload"]], [])
        with patch.object(camera, "active", return_value=True):
            camera.set_record({"path": "front", "mode": "armed"}, found)
        self.assertIn(["systemctl", "reload", "--", "uxduck-companion.service"], self.calls())
        for change, why in [({"path": "front", "mode": "sometimes"}, "armed, always or off"),
                            ({"path": "side", "mode": "off"}, "does not record side"),
                            ({"path": 'front"\nrecord = "off', "mode": "off"}, "does not record")]:
            with self.assertRaisesRegex(ValueError, why):
                camera.set_record(change, found)
        self.assertIn('[cameras.front]\nrecord = "armed"\n', path.read_text())
        # uxduck companion refuses a record line in a table with no record_path, so one
        # removed since the last read is never written.
        path.write_text('[cameras.front]\nfollow = true\n')
        with self.assertRaisesRegex(ValueError, "has no record_path"):
            camera.set_record({"path": "front", "mode": "off"}, found)
        self.assertEqual(path.read_text(), '[cameras.front]\nfollow = true\n')

    def microphones_from(self, case):
        """A copy of one fake microphone tree as this computer's /proc, and its /sys built from
        devices.json: each card's device folder and the link to it. The repository keeps no sys
        tree, because a USB device's folder has a colon in its name, which Windows cannot check out."""
        shutil.rmtree(self.root / "proc", ignore_errors=True)
        shutil.rmtree(self.root / "sys", ignore_errors=True)
        shutil.copytree(MICROPHONES / case / "proc", self.root / "proc")
        devices = json.loads((MICROPHONES / case / "devices.json").read_text())
        for card, device in devices.items():
            (self.root / "sys/devices" / device).mkdir(parents=True)
            (self.root / "sys/class/sound" / card).mkdir(parents=True)
            os.symlink(f"../../../devices/{device}", self.root / "sys/class/sound" / card / "device")

    def test_microphones_are_found_as_every_shared_fake_tree_expects(self):
        cases = sorted(p.name for p in MICROPHONES.iterdir() if p.is_dir())
        self.assertEqual(cases, ["loopback-only", "none", "one-usb", "several"])
        for case in cases:
            with self.subTest(case):
                self.microphones_from(case)
                expected = json.loads((MICROPHONES / case / "expected.json").read_text())
                found = camera.microphones()
                self.assertEqual(found, expected["microphones"])
                self.assertEqual(camera.automatic(found), expected["automatic"])
        # A computer with no sound cards at all.
        shutil.rmtree(self.root / "proc")
        self.assertEqual(camera.microphones(), [])

    def test_a_microphone_plugged_in_or_pulled_out_is_seen_on_the_next_read(self):
        self.microphones_from("one-usb")
        self.assertEqual(camera.automatic(camera.microphones()), "usb:3-1.2")
        # Pulled out: the kernel drops its card.
        cards = self.root / "proc/asound/cards"
        cards.write_text("".join(cards.read_text().splitlines(keepends=True)[:4]))
        shutil.rmtree(self.root / "proc/asound/card2")
        (self.root / "sys/class/sound/card2/device").unlink()
        self.assertEqual(camera.microphones(), [])
        self.assertIsNone(camera.automatic([]))
        # A Blue Snowball plugged into another port takes the next free card number.
        with cards.open("a") as file:
            file.write(" 2 [Snowball       ]: USB-Audio - Blue Snowball\n"
                       "                      BLUE MICROPHONE Blue Snowball at usb-xhci-hcd.0-2, full speed\n")
        (self.root / "proc/asound/card2/pcm0c").mkdir(parents=True)
        (self.root / "proc/asound/card2/id").write_text("Snowball\n")
        (self.root / "sys/devices/usb1/1-2/1-2:1.0").mkdir(parents=True)
        os.symlink("../../../devices/usb1/1-2/1-2:1.0", self.root / "sys/class/sound/card2/device")
        found = camera.microphones()
        self.assertEqual(found, [{"id": "usb:1-2", "name": "Blue Snowball", "card": "Snowball", "usb": True,
                                  "alsa": "hw:CARD=Snowball,DEV=0"}])
        self.assertEqual(camera.automatic(found), "usb:1-2")

    def test_each_camera_has_its_microphone_choice_and_the_device_it_gives_now(self):
        self.microphones_from("several")
        self.companion_toml('[cameras.front]\nfollow = true\n\n[cameras.rear]\nmicrophone = "none"\n\n'
                            '[cameras.side]\nmicrophone = "usb:9-9"  # on the mast\n\n[cameras.top]\nmicrophone = "card:Loopback"\n')
        status = {"paths": [{"name": n} for n in ("front", "rear", "side", "top", "relay")]}
        with patch.object(camera, "GSTREAMER", "uxduck-test-no-gst-launch"):
            camera.sound(status)
        self.assertEqual(status["microphones"], [
            {"id": "usb:3-1.2", "name": "USB PnP Sound Device", "card": "Device", "usb": True},
            {"id": "card:Loopback", "name": "Loopback PCM", "card": "Loopback", "usb": False},
            {"id": "usb:1-2", "name": "Blue Snowball", "card": "Snowball", "usb": True}])
        self.assertEqual([p.get("microphone") for p in status["paths"]], [
            {"choice": "auto", "device": "usb:3-1.2"}, {"choice": "none"}, {"choice": "usb:9-9"},
            {"choice": "card:Loopback", "device": "card:Loopback"}, None])
        self.assertIn("Sound needs GStreamer on this computer", status["soundProblem"])
        self.assertIn("gstreamer1.0-rtsp", status["soundProblem"])
        self.assertNotIn(";", status["soundProblem"])
        # With gst-launch-1.0 on the computer nothing is said.
        gst = self.fake / "bin/gst-launch-1.0"
        gst.write_text("#!/bin/sh\n")
        gst.chmod(0o755)
        status = {"paths": [{"name": "front"}]}
        camera.sound(status)
        self.assertNotIn("soundProblem", status)
        # Automatic with no microphone plugged in, or None, needs no GStreamer.
        self.microphones_from("none")
        self.companion_toml('[cameras.front]\n\n[cameras.rear]\nmicrophone = "none"\n')
        status = {"paths": [{"name": "front"}, {"name": "rear"}]}
        with patch.object(camera, "GSTREAMER", "uxduck-test-no-gst-launch"):
            camera.sound(status)
        self.assertEqual(status, {"paths": [{"name": "front", "microphone": {"choice": "auto"}},
                                            {"name": "rear", "microphone": {"choice": "none"}}], "microphones": []})
        # A chosen device that is not plugged in still wants sound.
        self.companion_toml('[cameras.front]\nmicrophone = "usb:1-2"\n')
        status = {"paths": [{"name": "front"}]}
        with patch.object(camera, "GSTREAMER", "uxduck-test-no-gst-launch"):
            camera.sound(status)
        self.assertIn("soundProblem", status)
        # No companion.toml: uxduck companion sends no camera, so none has a microphone.
        (self.root / "uxduck/companion.toml").unlink()
        status = {"paths": [{"name": "front"}]}
        camera.sound(status)
        self.assertEqual(status, {"paths": [{"name": "front"}], "microphones": []})

    def test_a_microphone_is_written_on_its_own_line_with_a_backup_and_every_other_line_is_kept(self):
        text = ('# Written by UXDuck\n[video]\ninterfaces = ["wlan0"]\n\n[cameras.front]\nrecord_path = "front_record"\nrecord = "armed"\n\n'
                '[cameras.rear]  # the back one\nfollow = false\nmicrophone = "auto" # default\n')
        path = self.companion_toml(text)
        found = {"paths": [{"name": "front", "microphone": {"choice": "auto"}}, {"name": "rear", "microphone": {"choice": "auto"}},
                           {"name": "relay"}],
                 "microphones": [{"id": "usb:3-1.2", "name": "USB PnP Sound Device", "card": "Device", "usb": True}]}
        backup = path.with_name("companion.toml.bak")
        camera.set_microphone({"path": "front", "choice": "usb:3-1.2"}, found)
        self.assertEqual(backup.read_text(), text)
        self.assertEqual(backup.stat().st_mode & 0o777, 0o640)
        once = text.replace('[cameras.front]\n', '[cameras.front]\nmicrophone = "usb:3-1.2"\n')
        self.assertEqual(path.read_text(), once)
        camera.set_microphone({"path": "rear", "choice": "none"}, found)
        self.assertEqual(backup.read_text(), once)
        twice = once.replace('microphone = "auto" # default\n', 'microphone = "none"\n')
        self.assertEqual(path.read_bytes(), twice.encode())
        self.assertEqual(path.stat().st_mode & 0o777, 0o640)
        # A microphone not plugged in now can be chosen by its id.
        camera.set_microphone({"path": "rear", "choice": "card:Snowball"}, found)
        self.assertIn('microphone = "card:Snowball"\n', path.read_text())
        self.assertEqual([c for c in self.calls() if c[:2] == ["systemctl", "reload"]], [])
        with patch.object(camera, "active", return_value=True):
            camera.set_microphone({"path": "rear", "choice": "auto"}, found)
        self.assertIn(["systemctl", "reload", "--", "uxduck-companion.service"], self.calls())
        kept = path.read_text()
        for change, why in [({"path": "front", "choice": "Blue Yeti"}, "Blue Yeti is not a microphone on this computer"),
                            ({"path": "front", "choice": 'usb:1-2"\nrecord = "off'}, "is not a microphone"),
                            ({"path": "front", "choice": "usb:"}, "is not a microphone"),
                            ({"path": "front", "choice": None}, "is not a microphone"),
                            ({"path": "relay", "choice": "auto"}, "does not send relay"),
                            ({"path": 'front]\nx = "', "choice": "auto"}, "does not send"),
                            ("front", "change is missing")]:
            with self.assertRaisesRegex(ValueError, why):
                camera.set_microphone(change, found)
        self.assertEqual(path.read_text(), kept)
        # A table removed since the last read is never added back.
        path.write_text('[cameras.rear]\n')
        with self.assertRaisesRegex(ValueError, r"has no \[cameras.front\] table"):
            camera.set_microphone({"path": "front", "choice": "none"}, found)
        self.assertEqual(path.read_text(), '[cameras.rear]\n')

    def test_a_reload_that_fails_says_the_microphone_is_saved(self):
        path = self.companion_toml('[cameras.front]\n')
        found = {"paths": [{"name": "front", "microphone": {"choice": "auto"}}]}
        with patch.object(camera, "active", return_value=True), \
                patch.object(camera, "run", side_effect=RuntimeError("systemctl reload failed: Job failed.")):
            with self.assertRaisesRegex(RuntimeError, "The microphone for front is saved as none in .*next starts"):
                camera.set_microphone({"path": "front", "choice": "none"}, found)
        self.assertEqual(path.read_text(), '[cameras.front]\nmicrophone = "none"\n')

    def test_drop_in_holds_only_overrides_and_keeps_other_cameras(self):
        self.drop_in.parent.mkdir(parents=True)
        self.drop_in.write_text(camera.render({("rear", "RPICAMERAFPS"): "30", ("cam", "RPICAMERAFPS"): "30"}))
        with patch.object(camera, "hand_over", return_value={"outcome": "applied"}) as hand_over:
            camera.apply({"path": "cam", "settings": self.low}, self.found(cam=("rpiCamera", False), rear=("rpiCamera", False), relay=("rtsp", True)))
        plan = hand_over.call_args.args[0]
        self.assertEqual(plan["paths"], ["cam", "rear"])
        lines = plan["content"].splitlines()
        self.assertEqual(lines[2], "[Service]")
        self.assertEqual(lines[3:], [
            'Environment="MTX_PATHS_CAM_RPICAMERABITRATE=2000000"',
            'Environment="MTX_PATHS_CAM_RPICAMERAFPS=50"',
            'Environment="MTX_PATHS_CAM_RPICAMERAHEIGHT=864"',
            'Environment="MTX_PATHS_CAM_RPICAMERAIDRPERIOD=50"',
            'Environment="MTX_PATHS_CAM_RPICAMERAMODE=2304:1296:10:P"',
            'Environment="MTX_PATHS_CAM_RPICAMERAWIDTH=1536"',
            'Environment="MTX_PATHS_REAR_RPICAMERAFPS=30"',
        ])
        self.assertEqual(camera.drop_in_values(self.drop_in)[("rear", "RPICAMERAFPS")], "30")
        for line in lines[3:]:
            self.assertRegex(line, camera.ENVIRONMENT_LINE)

    def test_bad_names_and_values_are_refused_before_anything_is_written(self):
        found = self.found(**{"cam": ("rpiCamera", False), "Cam": ("rpiCamera", False), "front_cam": ("rpiCamera", False),
                              "cam-1": ("rpiCamera", False), "relay": ("rtsp", False)})
        refusals = {
            "Cam": "lower case", "front_cam": "first _", "cam-1": "letters, digits", "cam/1": "letters, digits",
            "nowhere": "no camera path", "relay": "Only a Raspberry Pi camera",
        }
        for name, why in refusals.items():
            with self.assertRaisesRegex(ValueError, why):
                camera.apply({"path": name, "settings": self.low}, found)
        bad = [({"fps": 0}, "1 to 120"), ({"fps": 121}, "1 to 120"), ({"width": 1537}, "even"), ({"height": 32}, "64 to 8192"),
               ({"bitrate": 30_000_000}, "25 Mbit"), ({"idrPeriod": 0}, "keyframe"), ({"fps": "50"}, "whole number"),
               ({"fps": True}, "whole number"), ({"mode": "2304x1296"}, "not a sensor mode"),
               ({"mode": '2304:1296:10:P"\nExecStartPre=/bin/sh -c id'}, "not a sensor mode"),
               ({"mode": "1920:1080:10:P"}, "no 1920×1080 mode"), ({"mode": "2304:1296:12:P"}, "10-bit"),
               ({"fps": 60}, "at most 56"), ({"mode": "", "width": 4608, "height": 2592, "fps": 30}, "cannot give")]
        for change, why in bad:
            with self.assertRaisesRegex(ValueError, why):
                camera.apply({"path": "cam", "settings": {**self.low, **change}}, found)
        self.assertFalse(self.drop_in.exists())
        self.assertFalse((self.root / "work").exists())
        self.assertEqual(self.calls(), [])
        unknown = dict(found, paths=[dict(p, sensor=None) for p in found["paths"]])
        self.assertEqual(camera.check_picture({**self.low, "mode": "", "width": 4056, "height": 3040, "fps": 10}, None)["width"], 4056)
        with patch.object(camera, "hand_over", return_value={}) as hand_over:
            camera.apply({"path": "cam", "settings": {**self.low, "mode": "", "width": 1920, "height": 1080, "fps": 100}}, unknown)
        hand_over.assert_called_once()

    def test_lingering_is_read_and_a_risky_change_needs_dismissing(self):
        (self.root / "login.defs").write_text("UID_MIN 1000\nSYS_UID_MAX 999\n")
        pi = pwd.struct_passwd(("pi", "x", 1000, 1000, "", "/home/pi", "/bin/bash"))
        system = pwd.struct_passwd(("mediamtx", "x", 998, 998, "", "/", "/usr/sbin/nologin"))
        with patch.object(camera.pwd, "getpwnam", side_effect=lambda name: {"pi": pi, "mediamtx": system}[name]):
            self.assertIsNone(camera.linger_of(""))
            self.assertIsNone(camera.linger_of("root"))
            self.assertIsNone(camera.linger_of("mediamtx"))
            self.assertEqual(camera.linger_of("pi"), {"user": "pi", "lingering": False, "removeIpc": True})
            (self.root / "logind.conf").write_text("[Login]\n#RemoveIPC=no\n")
            self.assertTrue(camera.linger_of("pi")["removeIpc"])
            (self.root / "logind.conf.d").mkdir()
            (self.root / "logind.conf.d/10-ipc.conf").write_text("[Login]\nRemoveIPC=no\n")
            self.assertFalse(camera.linger_of("pi")["removeIpc"])
            (self.root / "logind.conf.d/10-ipc.conf").unlink()
            (self.root / "linger").mkdir()
            (self.root / "linger/pi").touch()
            self.assertTrue(camera.linger_of("pi")["lingering"])
        found = dict(self.found(cam=("rpiCamera", False)), linger={"user": "pi", "lingering": False, "removeIpc": True})
        with self.assertRaisesRegex(ValueError, "Turn lingering on"):
            camera.apply({"path": "cam", "settings": self.low}, found)
        with patch.object(camera, "hand_over", return_value={}) as hand_over:
            camera.apply({"path": "cam", "settings": self.low, "despiteLinger": True}, found)
        hand_over.assert_called_once()
        camera.enable_linger(found)
        self.assertEqual(self.calls(), [["loginctl", "enable-linger", "--", "pi"]])
        for user in ("-rf", "pi; reboot"):
            with self.assertRaises(ValueError):
                camera.enable_linger(dict(found, linger=dict(found["linger"], user=user)))

    def test_a_change_that_brings_every_camera_back_is_kept(self):
        cam, rear = Growing(), Growing()
        self.api.live = lambda: [cam.item("cam"), rear.item("rear")]
        work = self.root / "work"
        work.mkdir()
        new = {("cam", "RPICAMERAFPS"): "50"}
        (work / "plan.json").write_text(json.dumps(self.plan(new)))
        camera.job(work)
        self.assertEqual(json.loads((work / "result.json").read_text()), {"path": "cam", "outcome": "applied"})
        self.assertEqual(self.drop_in.read_text(), camera.render(new))
        self.assertEqual([c for c in self.calls() if c[0] == "systemctl"], [
            ["systemctl", "daemon-reload"], ["systemctl", "restart", "--", "mediamtx.service"]])

    def test_a_camera_that_never_comes_back_is_rolled_back_with_its_log(self):
        self.drop_in.parent.mkdir(parents=True)
        old = camera.render({("cam", "RPICAMERAFPS"): "30"})
        self.drop_in.write_text(old)
        cam, rear = Growing(), Growing()
        def live():
            # The new settings stop the camera, and the old ones bring it back.
            cam.ready = cam.growing = "RPICAMERAFPS=30" in self.drop_in.read_text()
            return [cam.item("cam"), rear.item("rear")]
        self.api.live = live
        (self.fake / "journal.txt").write_text(
            "INF [path rear] [RPI Camera source] started\n"
            "ERR [path relay] [RTSP source] rtsp://viewer:hunter2@10.0.0.9/stream: timed out\n"
            "ERR [path cam] [RPI Camera source] camera_create(): selected camera mode is not supported\n"
            "INF [path cam] [RPI Camera source] process exited\n")
        work = self.root / "work"
        work.mkdir()
        (work / "plan.json").write_text(json.dumps(self.plan({("cam", "RPICAMERAFPS"): "56"})))
        camera.job(work)
        result = json.loads((work / "result.json").read_text())
        self.assertEqual(result["outcome"], "rolled-back")
        self.assertEqual(result["failed"], "cam")
        self.assertEqual(result["log"], [
            "ERR [path cam] [RPI Camera source] camera_create(): selected camera mode is not supported",
            "INF [path cam] [RPI Camera source] process exited"])
        self.assertEqual(self.drop_in.read_text(), old)
        self.assertEqual((self.fake / "restarts").read_text(), "2")
        self.assertEqual(camera.journal("mediamtx.service", "nowhere", 0)[1], "ERR [path relay] [RTSP source] rtsp://10.0.0.9/stream: timed out")

    def test_a_first_change_that_fails_removes_the_drop_in_and_says_when_rollback_fails_too(self):
        self.api.live = lambda: [Growing(ready=False).item("cam"), Growing().item("rear")]
        work = self.root / "work"
        work.mkdir()
        (work / "plan.json").write_text(json.dumps(self.plan({("cam", "RPICAMERAFPS"): "56"})))
        camera.job(work)
        result = json.loads((work / "result.json").read_text())
        self.assertEqual(result["outcome"], "rollback-failed")
        self.assertFalse(self.drop_in.exists())

    def test_apply_hands_the_change_to_a_transient_unit_that_runs_the_job(self):
        cam = Growing()
        self.api.live = lambda: [cam.item("cam")]
        with patch.object(camera, "READY_WAIT_S", 20.0):
            result = camera.apply({"path": "cam", "settings": self.low}, self.found(cam=("rpiCamera", False)))
        self.assertEqual(result, {"path": "cam", "outcome": "applied"})
        run = next(c for c in self.calls() if c[0] == "systemd-run")
        self.assertEqual(run[1:7], ["--unit", "uxduck-camera-change", "--collect", "--wait", "--quiet", sys.executable])
        self.assertEqual(oct((self.root / "work").stat().st_mode & 0o777), "0o700")
        self.assertIn("MTX_PATHS_CAM_RPICAMERAMODE=2304:1296:10:P", self.drop_in.read_text())

    def test_read_over_ssh_lists_paths_without_their_addresses(self):
        config = self.root / "mediamtx.yml"
        config.write_text("logLevel: info\napi: yes\napiAddress: " + self.api.address + "  # local only\npaths:\n  cam:\n    source: rpiCamera\n")
        (self.fake / "units.txt").write_text("mediamtx.service loaded active running MediaMTX\nssh.service loaded active running OpenSSH\n")
        (self.fake / "show.txt").write_text(
            "Id=mediamtx.service\nExecStart={ path=/usr/local/bin/mediamtx ; argv[]=/usr/local/bin/mediamtx " + str(config) +
            " ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\nUser=\nWorkingDirectory=\nEnvironment=\n\n"
            "Id=ssh.service\nExecStart={ path=/usr/sbin/sshd ; argv[]=/usr/sbin/sshd -D ; ignore_errors=no }\nUser=\nWorkingDirectory=\nEnvironment=\n")
        sensor = self.root / "video4linux/video0"
        sensor.mkdir(parents=True)
        (sensor / "name").write_text("imx708_wide\n")
        self.api.configured = [
            {"name": "cam", "source": "rpiCamera", "rpiCameraCamID": 0, "rpiCameraWidth": 1536, "rpiCameraHeight": 864,
             "rpiCameraFPS": 50.0, "rpiCameraMode": "2304:1296:10:P", "rpiCameraIDRPeriod": 50, "rpiCameraBitrate": 2000000},
            {"name": "relay", "source": "rtsp://viewer:hunter2@10.0.0.9/stream", "sourceOnDemand": True},
            {"name": "~^live/.*$", "source": "publisher"}]
        self.api.live = lambda: [{"name": "cam", "ready": True, "readers": [{"type": "rtspSession"}], "inboundBytes": 123456}]
        # The adapter as SSH runs it, with python3 -c, reading the test's folder in place of the system's.
        self.microphones_from("one-usb")
        self.companion_toml('[cameras.cam]\nfollow = true\n')
        def here(value):
            if isinstance(value, tuple):
                return "(" + "".join(here(v) + ", " for v in value) + ")"
            return "pathlib.Path(" + repr(str(value)) + ")"
        places = {name: here(getattr(camera, name)) for name in
                  ("SYSTEMD", "LINGER", "LOGIND", "LOGIND_DIRS", "LOGIN_DEFS", "SENSOR_NAMES", "WATCHDOG_SCRIPT", "WATCHDOG_STATE",
                   "COMPANION_TOML", "ASOUND", "SOUND_CLASS")}
        source = "".join(next((name + " = " + value + "\n" for name, value in places.items() if line.startswith(name + " = ")), line)
                         for line in (HERE / "camera.py").read_text().splitlines(keepends=True))
        self.assertEqual(source.count(str(self.root)), len(places))
        result = subprocess.run([sys.executable, "-c", source], input="{}", capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("hunter2", result.stdout)
        line = next(line for line in result.stdout.splitlines() if line.startswith("cameras="))
        found = json.loads(line.removeprefix("cameras="))
        self.assertEqual(found["unit"], "mediamtx.service")
        self.assertEqual(found["config"], str(config))
        self.assertEqual(found["api"], self.api.address)
        self.assertEqual(found["paths"], [
            {"name": "cam", "source": "rpiCamera", "onDemand": False, "ready": True, "readers": 1, "bytes": 123456, "cameraId": 0,
             "settings": {"width": 1536, "height": 864, "fps": 50, "mode": "2304:1296:10:P", "idrPeriod": 50, "bitrate": 2000000},
             "setByUxduck": False, "sensor": "imx708", "microphone": {"choice": "auto", "device": "usb:3-1.2"}},
            {"name": "relay", "source": "rtsp", "onDemand": True, "ready": False, "readers": 0, "bytes": 0}])
        self.assertEqual(found["microphones"], [{"id": "usb:3-1.2", "name": "USB PnP Sound Device", "card": "Device", "usb": True}])
        self.assertNotIn("linger", found)

    def test_api_address_follows_the_yaml_and_the_environment(self):
        config = self.root / "mediamtx.yml"
        config.write_text("api: no\napiAddress: :9997\n")
        self.assertIsNone(camera.api_of(str(config), []))
        self.assertEqual(camera.api_of(str(config), ["MTX_API=yes"]), "127.0.0.1:9997")
        self.assertEqual(camera.api_of(str(config), ["MTX_API=true", "MTX_APIADDRESS=[::1]:9998"]), "[::1]:9998")
        config.write_text("api: yes\napiAddress: '0.0.0.0:9000'\n")
        self.assertEqual(camera.api_of(str(config), []), "127.0.0.1:9000")
        self.assertIsNone(camera.api_of(str(config), ["MTX_APIADDRESS=evil host:1"]))

    def test_one_change_at_a_time(self):
        with patch.object(sys, "stdin") as stdin:
            stdin.read.return_value = json.dumps({"linger": True, "watchdog": "install"})
            with self.assertRaisesRegex(ValueError, "one camera change"):
                camera.main()
        self.assertEqual(self.calls(), [])

    def test_watchdog_install_writes_its_unit_and_remove_deletes_everything(self):
        found = self.found(cam=("rpiCamera", False))
        script = (HERE / "camera_watchdog.py").read_text()
        with self.assertRaisesRegex(ValueError, "script is missing"):
            camera.install_watchdog(found, "print('hi')")
        with self.assertRaisesRegex(ValueError, "cannot write a watchdog"):
            camera.install_watchdog(dict(found, unit="mediamtx.service\nExecStartPre=/bin/sh"), script)
        camera.install_watchdog(found, script)
        unit = (self.root / "systemd/uxduck-camera-watchdog.service").read_text()
        self.assertIn("ExecStart=" + sys.executable + " " + str(self.root / "lib/camera-watchdog.py") +
                      " --unit mediamtx.service --api " + self.api.address + "\n", unit)
        self.assertIn("Restart=always\n", unit)
        self.assertEqual(oct((self.root / "lib/camera-watchdog.py").stat().st_mode & 0o777), "0o755")
        self.assertEqual(camera.watchdog_status(), {"installed": True, "running": False, "gaveUp": False, "restarts": []})
        state = self.root / "run/camera-watchdog.json"
        state.parent.mkdir()
        state.write_text(json.dumps({"gaveUp": True, "restarts": [{"at": 1700000000.5, "path": "cam"}, {"at": "bad"}]}))
        self.assertEqual(camera.watchdog_status()["restarts"], [{"atUs": 1700000000500000, "path": "cam"}])
        self.assertTrue(camera.watchdog_status()["gaveUp"])
        camera.remove_watchdog()
        self.assertFalse((self.root / "systemd/uxduck-camera-watchdog.service").exists())
        self.assertFalse((self.root / "lib/camera-watchdog.py").exists())
        self.assertFalse(state.exists())
        self.assertIn(["systemctl", "disable", "--now", "--", "uxduck-camera-watchdog.service"], self.calls())


def mp4_box(kind, body):
    return (8 + len(body)).to_bytes(4, "big") + kind + body


def mp4_full(kind, version, flags, body):
    return mp4_box(kind, bytes([version]) + flags.to_bytes(3, "big") + body)


def mp4_init():
    tkhd = mp4_full(b"tkhd", 0, 3, b"\0" * 8 + (1).to_bytes(4, "big") + b"\0" * 68)
    mdhd = mp4_full(b"mdhd", 0, 0, b"\0" * 8 + (90000).to_bytes(4, "big") + b"\0" * 8)
    hdlr = mp4_full(b"hdlr", 0, 0, b"\0" * 4 + b"vide" + b"\0" * 13)
    return mp4_box(b"ftyp", b"iso6\0\0\0\0") + mp4_box(b"moov", mp4_box(b"trak", tkhd + mp4_box(b"mdia", mdhd + hdlr)))


def mp4_fragment(base, durations):
    tfhd = mp4_full(b"tfhd", 0, 0x020000, (1).to_bytes(4, "big"))
    tfdt = mp4_full(b"tfdt", 1, 0, base.to_bytes(8, "big"))
    trun = mp4_full(b"trun", 0, 0x100, len(durations).to_bytes(4, "big") + b"".join(d.to_bytes(4, "big") for d in durations))
    moof = mp4_box(b"moof", mp4_full(b"mfhd", 0, 0, (1).to_bytes(4, "big")) + mp4_box(b"traf", tfhd + tfdt + trun))
    return moof + mp4_box(b"mdat", b"x" * 10)


class RecordingsOnTheVehicleTest(unittest.TestCase):
    """S429: the record path's segments, as recordings a person thinks of."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name)
        self.conf = {"recordPath": str(self.root) + "/%path/%Y-%m-%d_%H-%M-%S-%f", "recordFormat": "fmp4", "record": False}

    def tearDown(self):
        self.temp.cleanup()

    def segment(self, stamp, seconds, age=600):
        folder = self.root / "front_record"
        folder.mkdir(exist_ok=True)
        path = folder / (stamp + ".mp4")
        path.write_bytes(mp4_init() + b"".join(mp4_fragment(i * 90000, [45000, 45000]) for i in range(seconds)))
        then = time.time() - age
        os.utime(path, (then, then))
        return path

    @staticmethod
    def at(stamp):
        day, clock = stamp.split("_")
        y, m, d = (int(v) for v in day.split("-"))
        hh, mm, ss, f = (int(v) for v in clock.split("-"))
        return int((time.mktime((y, m, d, hh, mm, ss, 0, 0, -1)) + f / 1e6) * 1e6)

    def test_segments_that_meet_are_one_recording_and_a_gap_starts_another(self):
        first = self.segment("2026-11-23_10-00-00-000000", 60)
        second = self.segment("2026-11-23_10-01-00-000000", 30)
        third = self.segment("2026-11-23_10-05-00-250000", 20)
        folder, recordings = camera.path_recordings("front_record", self.conf, ready=True)
        self.assertEqual(folder, str(self.root / "front_record"))
        self.assertEqual(len(recordings), 2)
        self.assertEqual(recordings[0]["id"], "front_record/2026-11-23_10-00-00-000000.mp4")
        self.assertEqual(recordings[0]["startedAtUs"], self.at("2026-11-23_10-00-00-000000"))
        self.assertEqual(recordings[0]["durationUs"], 90_000_000)
        self.assertEqual(recordings[0]["segments"], [{"path": str(first), "size": first.stat().st_size},
                                                     {"path": str(second), "size": second.stat().st_size}])
        self.assertEqual(recordings[1]["segments"], [{"path": str(third), "size": third.stat().st_size}])
        self.assertEqual(recordings[1]["durationUs"], 20_000_000)
        self.assertFalse(recordings[1]["recording"])

    def test_the_newest_segment_is_still_recording_while_mediamtx_records_or_it_grows(self):
        self.segment("2026-11-23_10-00-00-000000", 5)
        _, recordings = camera.path_recordings("front_record", dict(self.conf, record=True), ready=True)
        self.assertTrue(recordings[-1]["recording"])
        _, recordings = camera.path_recordings("front_record", self.conf, ready=False)
        self.assertFalse(recordings[-1]["recording"])
        self.segment("2026-11-23_10-00-05-000000", 1, age=0)
        _, recordings = camera.path_recordings("front_record", self.conf, ready=False)
        self.assertEqual(len(recordings), 1)
        self.assertTrue(recordings[0]["recording"])

    def test_an_empty_segment_a_full_disk_left_is_not_a_recording(self):
        self.segment("2026-11-23_10-00-00-000000", 5)
        (self.root / "front_record" / "2026-11-23_11-00-00-000000.mp4").write_bytes(b"")
        old = time.time() - 600
        os.utime(self.root / "front_record" / "2026-11-23_11-00-00-000000.mp4", (old, old))
        _, recordings = camera.path_recordings("front_record", self.conf, ready=False)
        self.assertEqual([len(r["segments"]) for r in recordings], [1])

    def test_mpegts_is_listed_but_not_offered_for_download(self):
        folder = self.root / "front_record"
        folder.mkdir()
        (folder / "2026-11-23_10-00-00-000000.ts").write_bytes(b"G" * 188)
        old = time.time() - 600
        os.utime(folder / "2026-11-23_10-00-00-000000.ts", (old, old))
        _, recordings = camera.path_recordings("front_record", dict(self.conf, recordFormat="mpegts"), ready=False)
        self.assertIn("recordFormat: fmp4", recordings[0]["notDownloadable"])

    def test_a_record_path_without_its_own_absolute_folder_is_refused(self):
        for pattern in ("./recordings/%path/%Y", "/var/lib/recordings/%Y-%m-%d", "/var/lib/%path"):
            with self.assertRaisesRegex(ValueError, "absolute folder"):
                camera.record_store("front_record", {"recordPath": pattern})

    def test_listing_reads_each_recording_cameras_path_from_mediamtx(self):
        self.segment("2026-11-23_10-00-00-000000", 3)
        found = {"api": "127.0.0.1:1", "paths": [{"name": "front", "record": {"path": "front_record", "mode": "armed"}}, {"name": "rear"}]}
        answers = {"/v3/config/paths/get/front_record": self.conf, "/v3/paths/get/front_record": {"ready": True}}
        with patch.object(camera, "get", lambda api, endpoint: answers[endpoint]):
            listed = camera.list_recordings(found)
        self.assertEqual([c["camera"] for c in listed["cameras"]], ["front"])
        self.assertEqual(listed["cameras"][0]["mode"], "armed")
        self.assertEqual(len(listed["cameras"][0]["recordings"]), 1)
        with patch.object(camera, "get", side_effect=OSError("refused")):
            listed = camera.list_recordings(found)
        self.assertIn("could not list front_record", listed["cameras"][0]["problem"])


if __name__ == "__main__":
    unittest.main()
