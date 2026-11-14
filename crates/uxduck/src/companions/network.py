"""NetworkManager adapter, sent over pinned SSH. Input is JSON on stdin.

Saving credentials never activates or deactivates a connection. NetworkManager
chooses an autoconnect profile when Wi-Fi is disconnected. Only UXDuck-owned
profiles are replaced, and passwords never appear in command arguments/output.
"""
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import uuid
from urllib.parse import urlsplit

ROOT = pathlib.Path("/etc/NetworkManager")
DBUS = ("org.freedesktop.NetworkManager", "/org/freedesktop/NetworkManager", "org.freedesktop.NetworkManager")

def prop(name):
    return run("busctl", "get-property", *DBUS, name)


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=12, env={**os.environ, "LC_ALL": "C"})
    if result.returncode:
        # nmcli can echo property values. Never return its raw stderr.
        raise RuntimeError("NetworkManager could not complete the request. Check its service and permissions.")
    # Leading/trailing spaces are meaningful in an SSID.
    return result.stdout.rstrip("\n")


def fields(line):
    """nmcli terse output escapes both backslashes and separators."""
    out, part, escape = [], "", False
    for char in line:
        if escape:
            part += char
            escape = False
        elif char == "\\":
            escape = True
        elif char == ":":
            out.append(part)
            part = ""
        else:
            part += char
    out.append(part)
    return out


def keyvalue(value):
    return value.replace("\\", "\\\\").replace(" ", "\\s").replace("\t", "\\t").replace("\n", "\\n").replace("\r", "\\r")


def replace(path, content, apply):
    """Atomic private write, restore the previous file if NM rejects it."""
    old = path.read_bytes() if path.exists() else None
    path.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    def write(data):
        fd, name = tempfile.mkstemp(prefix=".uxduck-", dir=path.parent)
        try:
            with os.fdopen(fd, "wb") as file:
                file.write(data)
                file.flush()
                os.fsync(file.fileno())
            os.replace(name, path)
        finally:
            if os.path.exists(name):
                os.unlink(name)
    write(content.encode())
    try:
        apply()
    except Exception:
        if old is None:
            path.unlink(missing_ok=True)
        else:
            write(old)
        # Re-read the restored configuration too.
        try:
            run("nmcli", "connection", "reload")
            run("nmcli", "general", "reload", "conf")
        except Exception:
            pass
        raise


def save_wifi(wifi):
    ssid = wifi["ssid"]
    if not isinstance(ssid, str) or not 1 <= len(ssid.encode()) <= 32 or any(ord(c) < 32 or ord(c) == 127 for c in ssid):
        raise ValueError("A Wi-Fi name must be 1 to 32 bytes without control characters.")
    security, password = wifi["security"], wifi["password"]
    if security not in ("open", "wpa-psk", "sae"):
        raise ValueError("Use open, WPA2 Personal or WPA3 Personal Wi-Fi.")
    hexkey = len(password) == 64 and all(c in "0123456789abcdefABCDEF" for c in password)
    if security == "open":
        if password:
            raise ValueError("Open networks have no password.")
    elif not (8 <= len(password.encode()) <= 63 or security == "wpa-psk" and hexkey) or any(ord(c) < 32 or ord(c) == 127 for c in password):
        raise ValueError("The Wi-Fi password has an invalid length or character.")
    priority = wifi["priority"]
    if not isinstance(priority, int) or not -999 <= priority <= 999:
        raise ValueError("Wi-Fi priority must be between -999 and 999.")
    ident = str(uuid.uuid5(uuid.NAMESPACE_DNS, "uxduck-wifi:" + ssid))
    path = ROOT / "system-connections" / ("uxduck-" + ident + ".nmconnection")
    # Bytes avoid GLib interpreting an SSID's punctuation as keyfile syntax.
    content = ("[connection]\nid=" + keyvalue("UXDuck " + ssid) + "\nuuid=" + ident +
               "\ntype=wifi\nautoconnect=true\nautoconnect-priority=" + str(priority) +
               "\nmetered=" + ("1" if wifi["metered"] else "2") +
               "\n[wifi]\nmode=infrastructure\nssid=" + ";".join(str(b) for b in ssid.encode()) + ";\n" +
               "hidden=" + ("true" if wifi["hidden"] else "false") + "\n")
    if security != "open":
        content += "[wifi-security]\nkey-mgmt=" + security + "\npsk=" + keyvalue(password) + "\n"
    content += "[ipv4]\nmethod=auto\nroute-metric=600\n[ipv6]\nmethod=auto\nroute-metric=600\n"
    replace(path, content, lambda: run("nmcli", "connection", "load", str(path)))


def set_connectivity(check):
    uri, interval = check["uri"], check["interval"]
    parsed = urlsplit(uri)
    if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or parsed.password or any(c.isspace() for c in uri):
        raise ValueError("Use an HTTP or HTTPS health-check address without credentials.")
    if not isinstance(interval, int) or interval != 0 and not 30 <= interval <= 3600:
        raise ValueError("Check every 30 to 3600 seconds, or use zero to disable checks.")
    content = "[connectivity]\nenabled=" + ("true" if interval else "false") + "\nuri=" + uri + "\ninterval=" + str(interval) + "\nresponse=\n"
    previous = prop("ConnectivityCheckEnabled") == "b true"
    enabled = "true" if interval else "false"
    def apply():
        run("nmcli", "general", "reload", "conf")
        # A previous D-Bus disable is persisted by NetworkManager and can
        # override a drop-in. Set its supported runtime property explicitly.
        run("busctl", "set-property", *DBUS, "ConnectivityCheckEnabled", "b", enabled)
        if prop("ConnectivityCheckEnabled") != "b " + enabled or json.loads(prop("ConnectivityCheckUri")[2:]) != uri:
            raise RuntimeError("The effective health-check configuration did not match.")
        if interval and prop("ConnectivityCheckAvailable") != "b true":
            raise RuntimeError("Internet health checks are not available.")
    try:
        replace(ROOT / "conf.d" / "90-uxduck-connectivity.conf", content, apply)
    except Exception:
        try:
            run("busctl", "set-property", *DBUS, "ConnectivityCheckEnabled", "b", "true" if previous else "false")
        except Exception:
            pass
        raise


def status():
    wifi = []
    for line in run("nmcli", "-t", "-f", "UUID,TYPE,ACTIVE", "connection", "show").splitlines():
        ident, kind, active = fields(line)
        if kind != "802-11-wireless":
            continue
        ssid = run("nmcli", "-g", "802-11-wireless.ssid", "connection", "show", "uuid", ident)
        # -g escapes colons/backslashes as terse output does.
        ssid = fields(ssid)[0]
        wifi.append({"id": ident, "ssid": ssid, "active": active == "yes", "managed": ident == str(uuid.uuid5(uuid.NAMESPACE_DNS, "uxduck-wifi:" + ssid))})
    return {"wifi": wifi, "connectivity": run("nmcli", "-t", "-f", "CONNECTIVITY", "general"),
            "checksEnabled": prop("ConnectivityCheckEnabled") == "b true",
            "checkUri": json.loads(prop("ConnectivityCheckUri")[2:])}


def main():
    request = json.load(sys.stdin)
    if request.get("wifi") and request.get("connectivity"):
        raise ValueError("Make one network change at a time.")
    # Fail before changing anything if NetworkManager or its D-Bus API is absent.
    status()
    if request.get("wifi"):
        save_wifi(request["wifi"])
    if request.get("connectivity"):
        set_connectivity(request["connectivity"])
    print("network=" + json.dumps(status(), ensure_ascii=True))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, RuntimeError, subprocess.TimeoutExpired):
        # No input, subprocess stderr, credentials or file contents in errors.
        print("Network setup failed. This needs Linux with NetworkManager, Python 3 and busctl. Saving needs passwordless sudo. Check the network values and companion service logs.", file=sys.stderr)
        sys.exit(1)
