import importlib.util
import pathlib
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("network", pathlib.Path(__file__).with_name("network.py"))
network = importlib.util.module_from_spec(spec)
spec.loader.exec_module(network)


class NetworkTests(unittest.TestCase):
    def test_command_output_preserves_ssid_spaces(self):
        with patch.object(network.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, " field\\:one \n", "")):
            self.assertEqual(network.fields(network.run("nmcli"))[0], " field:one ")

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.patch = patch.object(network, "ROOT", self.root)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.wifi = {"ssid": "Field: \\ 5G", "security": "wpa-psk", "password": " secret;#=\\word ", "priority": 12, "hidden": True, "metered": True}

    def test_saving_is_private_and_never_switches_the_live_connection(self):
        with patch.object(network, "run", return_value="") as run:
            network.save_wifi(self.wifi)
            path = next((self.root / "system-connections").iterdir())
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            self.assertEqual(run.call_args.args[:3], ("nmcli", "connection", "load"))
            self.assertEqual(run.call_count, 1)
            self.assertNotIn(self.wifi["password"], str(run.call_args))
            text = path.read_text()
            self.assertIn("ssid=70;105;101;108;100;58;32;92;32;53;71;", text)
            self.assertIn("psk=\\ssecret;#=\\\\word\\s", text)
            self.assertIn("autoconnect=true", text)
            # Same SSID updates only the UXDuck-created profile.
            self.wifi["password"] = "new-password"
            network.save_wifi(self.wifi)
            self.assertEqual(len(list(path.parent.iterdir())), 1)
            self.assertIn("new-password", path.read_text())

    def test_rejected_profile_restores_previous_credentials(self):
        with patch.object(network, "run", return_value=""):
            network.save_wifi(self.wifi)
        path = next((self.root / "system-connections").iterdir())
        before = path.read_bytes()
        self.wifi["password"] = "new-password"
        with patch.object(network, "run", side_effect=RuntimeError("rejected")):
            with self.assertRaises(RuntimeError):
                network.save_wifi(self.wifi)
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_health_checks_reload_configuration_without_restart(self):
        values = {"ConnectivityCheckEnabled": "b false", "ConnectivityCheckUri": 's "https://example.test/204"', "ConnectivityCheckAvailable": "b true"}
        def bus(*args):
            if args[:2] == ("busctl", "set-property"):
                values["ConnectivityCheckEnabled"] = "b " + args[-1]
            if args[:2] == ("busctl", "get-property"):
                return values[args[-1]]
            return ""
        with patch.object(network, "run", side_effect=bus) as run:
            network.set_connectivity({"uri": "https://example.test/204", "interval": 60})
            self.assertEqual(values["ConnectivityCheckEnabled"], "b true")
            run.assert_any_call("nmcli", "general", "reload", "conf")
            self.assertFalse(any("restart" in call.args for call in run.call_args_list))
        text = (self.root / "conf.d/90-uxduck-connectivity.conf").read_text()
        self.assertIn("enabled=true\n", text)
        self.assertIn("interval=60\nresponse=\n", text)
        for uri in ["https://host/\n[main]\nplugins=bad", "https://user:secret@host/204", "file:///tmp/x"]:
            with self.assertRaises(ValueError):
                network.set_connectivity({"uri": uri, "interval": 60})
        self.assertEqual((self.root / "conf.d/90-uxduck-connectivity.conf").read_text(), text)

    def test_conflicting_health_check_config_is_rolled_back(self):
        enabled = False
        def bus(*args):
            nonlocal enabled
            if args[:2] == ("busctl", "set-property"):
                enabled = args[-1] == "true"
            if args[-1] == "ConnectivityCheckEnabled":
                return "b true" if enabled else "b false"
            if args[-1] == "ConnectivityCheckUri":
                return 's "https://policy.example/204"'
            return ""
        with patch.object(network, "run", side_effect=bus):
            with self.assertRaises(RuntimeError):
                network.set_connectivity({"uri":"https://example.test/204", "interval":60})
        self.assertFalse(enabled)
        self.assertFalse((self.root / "conf.d/90-uxduck-connectivity.conf").exists())

    def test_status_parses_escaped_ssids_without_reading_secrets(self):
        def run(*args):
            if "UUID,TYPE,ACTIVE" in args:
                return "id:802-11-wireless:yes"
            if "802-11-wireless.ssid" in args:
                return r"field\:one\\two"
            if "CONNECTIVITY" in args:
                return "limited"
            if args[-1] == "ConnectivityCheckEnabled":
                return "b false"
            if args[-1] == "ConnectivityCheckUri":
                return 's ""'
            self.fail(str(args))
        with patch.object(network, "run", side_effect=run):
            status = network.status()
        self.assertEqual(status["wifi"][0]["ssid"], "field:one\\two")
        self.assertTrue(status["wifi"][0]["active"])
        self.assertFalse(status["checksEnabled"])


if __name__ == "__main__":
    unittest.main()
