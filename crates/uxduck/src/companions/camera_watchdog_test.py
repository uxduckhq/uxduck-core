import http.server
import importlib.util
import json
import os
import pathlib
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("camera_watchdog", pathlib.Path(__file__).with_name("camera_watchdog.py"))
camera_watchdog = importlib.util.module_from_spec(spec)
spec.loader.exec_module(camera_watchdog)

# A stand-in for systemctl that records each call.
SYSTEMCTL = """#!PYTHON
import json, os, sys
with open(os.environ["FAKE_CALLS"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\\n")
"""


class FakeApi:
    """MediaMTX's local API with one path whose state the test sets."""

    def __init__(self):
        self.configured, self.live, self.down = [], [], False
        api = self

        class Handler(http.server.BaseHTTPRequestHandler):
            disable_nagle_algorithm = True

            def do_GET(self):
                if api.down:
                    self.send_error(503)
                    return
                items = api.configured if self.path.startswith("/v3/config/paths/list") else api.live
                body = json.dumps({"items": items}).encode()
                self.send_response(200)
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


class WatchdogTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = pathlib.Path(temp.name)
        (root / "bin").mkdir()
        systemctl = root / "bin/systemctl"
        systemctl.write_text(SYSTEMCTL.replace("PYTHON", sys.executable, 1))
        systemctl.chmod(0o755)
        self.calls = root / "calls.jsonl"
        environment = patch.dict(os.environ, {"PATH": str(root / "bin") + os.pathsep + os.environ["PATH"], "FAKE_CALLS": str(self.calls)})
        environment.start()
        self.addCleanup(environment.stop)
        self.api = FakeApi()
        self.addCleanup(self.api.close)
        self.state = root / "run/camera-watchdog.json"
        self.watchdog = camera_watchdog.Watchdog("mediamtx.service", self.api.address, self.state)
        self.camera("cam", 1000)
        logged = patch.object(camera_watchdog, "log")
        self.log = logged.start()
        self.addCleanup(logged.stop)

    def camera(self, name, received, source="rpiCamera", on_demand=False, readers=0, ready=True):
        self.api.configured = [{"name": name, "source": source, "sourceOnDemand": on_demand}]
        self.api.live = [{"name": name, "ready": ready, "readers": [{}] * readers, "inboundBytes": received}]

    def restarts(self):
        if not self.calls.exists():
            return 0
        return sum(1 for line in self.calls.read_text().splitlines() if json.loads(line) == ["restart", "--", "mediamtx.service"])

    def run_for(self, start, end, grow=0):
        """Polls every 2 s from start to end seconds, the camera's bytes growing by grow each time."""
        for second in range(start, end + 1, 2):
            if grow:
                self.api.live[0]["inboundBytes"] += grow
            self.watchdog.step(float(second), 1_700_000_000.0 + second)

    def test_a_frozen_camera_restarts_mediamtx_once(self):
        self.run_for(0, 8)
        self.assertEqual(self.restarts(), 0)
        self.run_for(10, 10)
        self.assertEqual(self.restarts(), 1)
        # After the restart the camera gets 10 s from its first poll again.
        self.run_for(12, 20)
        self.assertEqual(self.restarts(), 1)
        state = json.loads(self.state.read_text())
        self.assertEqual(state["restarts"], [{"at": 1_700_000_010.0, "path": "cam"}])
        self.assertEqual(state["judged"], ["cam"])
        self.assertFalse(state["gaveUp"])
        self.assertIn("pauses every camera", self.log.call_args_list[0].args[0])

    def test_it_stops_after_three_restarts_in_ten_minutes(self):
        self.run_for(0, 598)
        self.assertEqual(self.restarts(), 3)
        state = json.loads(self.state.read_text())
        self.assertTrue(state["gaveUp"])
        self.assertEqual(len(state["restarts"]), 3)
        self.assertIn("stops restarting", self.log.call_args_list[-1].args[0])
        self.run_for(600, 1800)
        self.assertEqual(self.restarts(), 3)

    def test_a_healthy_camera_is_never_restarted(self):
        self.run_for(0, 1200, grow=40_000)
        self.assertEqual(self.restarts(), 0)
        self.assertEqual(json.loads(self.state.read_text())["judged"], ["cam"])

    def test_an_idle_on_demand_camera_a_camera_already_down_and_other_sources_are_not_judged(self):
        self.camera("cam", 1000, on_demand=True)
        self.run_for(0, 120)
        self.camera("relay", 1000, source="rtsp://10.0.0.9/stream")
        self.run_for(122, 240)
        self.assertEqual(self.restarts(), 0)
        self.assertEqual(json.loads(self.state.read_text())["judged"], [])
        # MediaMTX retries a camera it already reports down by itself.
        self.camera("cam", 0, ready=False)
        self.run_for(242, 360)
        self.assertEqual(self.restarts(), 0)
        self.camera("cam", 1000, on_demand=True, readers=1)
        self.run_for(362, 372)
        self.assertEqual(self.restarts(), 1)

    def test_nothing_is_judged_while_mediamtx_does_not_answer(self):
        self.api.down = True
        self.run_for(0, 60)
        self.assertEqual(self.restarts(), 0)
        self.api.down = False
        self.run_for(62, 70)
        self.assertEqual(self.restarts(), 0)


if __name__ == "__main__":
    unittest.main()
