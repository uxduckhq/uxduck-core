import importlib.util
import json
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
        for key, path in (("PREFERENCE", self.root / "preference.json"), ("PENDING", self.root / "pending.json")):
            patcher = patch.object(network, key, path)
            patcher.start()
            self.addCleanup(patcher.stop)
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
            if "UUID,TYPE,ACTIVE,NAME,DEVICE" in args:
                return "id:802-11-wireless:yes:Office:wlan0"
            if "GENERAL.IP4-CONNECTIVITY,GENERAL.IP6-CONNECTIVITY" in args:
                return "3 (limited)\n1 (none)"
            if args[0] == "ip":
                return '[{"dev":"wlan0","metric":600}]' if "-4" in args else "[]"
            if args[-1] == "ConnectivityCheckAvailable":
                return "b false"
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

    def routing(self):
        self.wifi_id = "11111111-1111-4111-8111-111111111111"
        self.cell_id = "22222222-2222-4222-8222-222222222222"
        self.preference = {"wifiId": self.wifi_id, "cellularId": self.cell_id}
        self.profiles = {ident: {"ipv4.route-metric": "-1", "ipv6.route-metric": "-1", "connection.autoconnect": "no", "connection.metered": "unknown"} for ident in (self.wifi_id, self.cell_id)}
        self.checkpoints = 'ao 1 "/checkpoint/1"'
        self.before = {"connections": [{"id": self.wifi_id, "kind": "802-11-wireless", "device": "wlan0"}, {"id": self.cell_id, "kind": "802-3-ethernet", "device": "usb0"}], "checksEnabled": False, "checkUri": ""}
        def run(*args):
            if args[:3] == ("nmcli", "connection", "modify"):
                self.profiles[args[4]].update(dict(zip(args[5::2], args[6::2])))
            elif args[:2] == ("nmcli", "-g"):
                if args[2].endswith(".never-default"): return "no"
                if args[2].endswith(".route-table"): return "0"
                return "\n".join(self.profiles[args[-1]][key] for key in args[2].split(","))
            elif args[-1] == "Checkpoints": return self.checkpoints
            elif args[-1] in ("ConnectivityCheckEnabled", "ConnectivityCheckAvailable"): return "b true"
            elif "CheckpointCreate" in args: return 'o "/checkpoint/1"'
            return ""
        return run

    def test_preference_waits_for_separate_confirmation(self):
        run = self.routing()
        with patch.object(network, "run", side_effect=run) as commands, patch.object(network, "set_connectivity") as checks, patch.object(network.subprocess, "Popen") as watchdog:
            token = network.set_preference(self.preference, self.before)
            self.assertFalse(network.PREFERENCE.exists())
            self.assertEqual(stat.S_IMODE(network.PENDING.stat().st_mode), 0o600)
            watchdog.assert_called_once()
            self.assertTrue(watchdog.call_args.kwargs["start_new_session"])
            checks.assert_called_once_with({"uri": "https://connectivitycheck.gstatic.com/generate_204", "interval": 60})
            commands.assert_any_call("nmcli", "device", "reapply", "wlan0")
            commands.assert_any_call("nmcli", "device", "reapply", "usb0")
            with self.assertRaises(ValueError): network.confirm_preference("wrong-token")
            self.assertTrue(network.PENDING.exists())
            network.confirm_preference(token)
            self.assertEqual(network.preference_status(), self.preference)
            self.assertFalse(network.PENDING.exists())
            commands.assert_any_call("busctl", "call", *network.DBUS, "CheckpointDestroy", "o", "/checkpoint/1")

    def test_expired_or_mismatched_preference_cannot_be_confirmed(self):
        run = self.routing()
        with patch.object(network, "run", side_effect=run), patch.object(network, "set_connectivity"), patch.object(network.subprocess, "Popen"):
            token = network.set_preference(self.preference, self.before)
            self.profiles[self.cell_id]["ipv6.route-metric"] = "500"
            with self.assertRaisesRegex(RuntimeError, "did not apply"): network.confirm_preference(token)
            self.checkpoints = "ao 0"
            with self.assertRaisesRegex(RuntimeError, "expired"): network.confirm_preference(token)
            self.assertFalse(network.PREFERENCE.exists())

    def test_apply_failure_and_watchdog_spawn_failure_restore_immediately(self):
        for failure in ("apply", "spawn"):
            run = self.routing()
            with patch.object(network, "run", side_effect=run), patch.object(network, "set_connectivity", side_effect=RuntimeError("apply") if failure == "apply" else None), patch.object(network.subprocess, "Popen", side_effect=OSError("spawn") if failure == "spawn" else None), patch.object(network, "restore_routing") as restore:
                with self.assertRaises((OSError, RuntimeError)):
                    network.set_preference(self.preference, self.before)
                restore.assert_called_once()
                self.assertEqual(restore.call_args.args[0]["profiles"][self.cell_id]["ipv6.route-metric"], "-1")
                self.assertFalse(network.PENDING.exists())

    def test_watchdog_preserves_failed_recovery_and_never_undoes_confirmed_changes(self):
        network.private_json(network.PENDING, {"token": "pending"})
        with patch.object(network.time, "sleep"), patch.object(network, "restore_routing", side_effect=RuntimeError("offline")) as restore:
            network.routing_watchdog(str(network.PENDING), "pending")
            self.assertEqual(restore.call_count, 3)
            self.assertTrue(network.PENDING.exists())
        with patch.object(network.time, "sleep"), patch.object(network, "restore_routing") as restore:
            network.routing_watchdog(str(network.PENDING), "different")
            restore.assert_not_called()
            network.routing_watchdog(str(network.PENDING), "pending")
            restore.assert_called_once()
            self.assertFalse(network.PENDING.exists())
            restore.reset_mock()
            network.routing_watchdog(str(network.PENDING), "pending")
            restore.assert_not_called()

    def test_restore_handles_an_already_expired_checkpoint_and_restores_files(self):
        saved = self.root / "conf"
        saved.write_text("modified")
        snapshot = {"profiles": {"id": {"ipv4.route-metric": "600"}}, "files": {str(saved): {"data": "b3JpZ2luYWw=", "mode": 0o600}}, "devices": ["wlan0"], "enabled": "false", "checkpoint": "/expired"}
        with patch.object(network.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "ao 0", "")) as commands:
            network.restore_routing(snapshot)
        self.assertEqual(saved.read_text(), "original")
        self.assertFalse(any("CheckpointRollback" in c.args[0] for c in commands.call_args_list))
        self.assertTrue(any("reapply" in c.args[0] for c in commands.call_args_list))

    def test_default_routes_include_ipv6_and_refuse_ambiguous_policy_routes(self):
        with patch.object(network, "run", side_effect=['[{"dev":"wlan0","metric":600},{"dev":"usb0","metric":700}]', '[{"dev":"usb0","metric":700}]']):
            self.assertEqual(network.default_routes(), (["usb0", "wlan0"], True))
        for routes in ('[{"dev":"wlan0"},{"dev":"usb0"}]', '[{"dev":"tailscale0","table":52}]', '[{"nexthops":[{"dev":"usb0"}]}]'):
            with patch.object(network, "run", side_effect=[routes, "[]"]):
                self.assertFalse(network.default_routes()[1])


if __name__ == "__main__":
    unittest.main()
