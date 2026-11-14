import http.server
import importlib.util
import json
import os
import pathlib
import pwd
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

HERE = pathlib.Path(__file__).parent
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
        def here(value):
            if isinstance(value, tuple):
                return "(" + "".join(here(v) + ", " for v in value) + ")"
            return "pathlib.Path(" + repr(str(value)) + ")"
        places = {name: here(getattr(camera, name)) for name in
                  ("SYSTEMD", "LINGER", "LOGIND", "LOGIND_DIRS", "LOGIN_DEFS", "SENSOR_NAMES", "WATCHDOG_SCRIPT", "WATCHDOG_STATE")}
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
             "setByUxduck": False, "sensor": "imx708"},
            {"name": "relay", "source": "rtsp", "onDemand": True, "ready": False, "readers": 0, "bytes": 0}])
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


if __name__ == "__main__":
    unittest.main()
