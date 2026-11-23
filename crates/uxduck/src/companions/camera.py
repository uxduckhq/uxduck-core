"""MediaMTX cameras on a companion computer (ADR-0060), sent over pinned SSH.

Input is JSON on stdin. {} reads. A change is one of
{"apply": {"path", "settings", "despiteLinger"}}, {"linger": true},
{"watchdog": "install" | "remove", "watchdogScript": "..."},
{"record": {"path", "mode"}} or {"microphone": {"path", "choice"}}, run as
root through sudo -n. {"recordings": true} lists each recording camera's
recordings on the vehicle instead, as "recordings=<json>" (S429). It prints
"cameras=<json>": the systemd unit running MediaMTX, its paths as MediaMTX's
local API has them, lingering for the unit's user, the camera watchdog, each
camera's recording on the vehicle and microphone from
/etc/uxduck/companion.toml (ADR-0066 R10, ADR-0067 M3), and the computer's
microphones (ADR-0067 M2).

A picture is written as MediaMTX's own environment overrides in one systemd
drop-in UXDuck owns, never into the person's YAML. The restart that applies
it, the wait for every camera and the rollback run in a transient systemd
unit, so they finish even if the SSH link drops. Nothing from the request is
put in a shell command: every value is checked, then passed as an argument
or written to a file.
"""
import json
import os
import pathlib
import pwd
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request

SYSTEMD = pathlib.Path("/etc/systemd/system")
DROP_IN = "60-uxduck-camera.conf"
LINGER = pathlib.Path("/var/lib/systemd/linger")
LOGIND = pathlib.Path("/etc/systemd/logind.conf")
LOGIND_DIRS = tuple(pathlib.Path(d) for d in ("/usr/lib/systemd/logind.conf.d", "/usr/local/lib/systemd/logind.conf.d", "/etc/systemd/logind.conf.d", "/run/systemd/logind.conf.d"))
LOGIN_DEFS = pathlib.Path("/etc/login.defs")
SENSOR_NAMES = tuple(pathlib.Path(d) for d in ("/sys/class/video4linux", "/sys/bus/i2c/devices"))
WORK = pathlib.Path("/run/uxduck/camera-change")
CHANGE_UNIT = "uxduck-camera-change"
WATCHDOG_SCRIPT = pathlib.Path("/usr/local/lib/uxduck/camera-watchdog.py")
WATCHDOG_UNIT = "uxduck-camera-watchdog.service"
WATCHDOG_STATE = pathlib.Path("/run/uxduck/camera-watchdog.json")
COMPANION_TOML = pathlib.Path("/etc/uxduck/companion.toml")
COMPANION_UNIT = "uxduck-companion.service"
RECORD_MODES = ("armed", "always", "off")
# Where the kernel lists sound cards (ADR-0067 M2): /proc/asound/cards, each
# card's pcm<N>c capture folders, and /sys/class/sound/card<N>/device.
ASOUND = pathlib.Path("/proc/asound")
SOUND_CLASS = pathlib.Path("/sys/class/sound")
CARD_LINE = re.compile(r"^\s*(\d+)\s+\[([^\]]*)\]:\s*.*? - (.*?)\s*$")
CAPTURE = re.compile(r"pcm(\d+)c")
USB_INTERFACE = re.compile(r"(\d+-\d+(?:\.\d+)*):\d+\.\d+")
# A device id as companion.toml keeps it: usb:<bus path> or card:<ALSA id>.
MICROPHONE_ID = re.compile(r"usb:[0-9]+-[0-9]+(?:\.[0-9]+)*|card:[A-Za-z0-9_-]+")
GSTREAMER = "gst-launch-1.0"
GSTREAMER_PACKAGES = "gstreamer1.0-tools, gstreamer1.0-alsa, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good and gstreamer1.0-rtsp"
# The [cameras.<path>] tables UXDuck reads and writes in companion.toml
# (ADR-0066 R10): standard tables of basic strings, one key to a line.
CAMERAS_TABLE = re.compile(r'^\s*\[\s*cameras\.(?:"([^"\\]+)"|([A-Za-z0-9_-]+))\s*\]\s*(?:#.*)?$')
ANY_TABLE = re.compile(r"^\s*\[")
STRING_KEY = re.compile(r'^\s*([A-Za-z0-9_-]+)\s*=\s*"([^"\\]*)"\s*(?:#.*)?$')
READY_WAIT_S = 20.0
POLL_S = 0.5
# The JSON field, and MediaMTX's field as it reads it from the environment.
FIELDS = (("width", "RPICAMERAWIDTH"), ("height", "RPICAMERAHEIGHT"), ("fps", "RPICAMERAFPS"),
          ("mode", "RPICAMERAMODE"), ("idrPeriod", "RPICAMERAIDRPERIOD"), ("bitrate", "RPICAMERABITRATE"))
# Camera Module 3 (IMX708): width, height and most pictures a second of each 10-bit mode.
SENSORS = {"imx708": ((1536, 864, 120), (2304, 1296, 56), (4608, 2592, 14))}
ENVIRONMENT_LINE = re.compile(r'^Environment="MTX_PATHS_([A-Z0-9]+)_(RPICAMERA[A-Z]+)=([0-9A-Z:]*)"$')
MODE = re.compile(r"^(\d{1,5}):(\d{1,5})(?::(\d{1,2})(?::[PU])?)?$")
# Built once: building an opener loads the system's certificates. Never through a proxy.
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def run(*args, timeout=15):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout,
                            env={**os.environ, "LC_ALL": "C", "SYSTEMD_PAGER": "", "SYSTEMD_COLORS": "0"})
    if result.returncode:
        detail = result.stderr.strip().splitlines()
        raise RuntimeError(" ".join(args[:2]) + " failed" + (": " + detail[-1][:300] if detail else "."))
    return result.stdout


def fetch(api, endpoint):
    """Items from MediaMTX's local API."""
    with OPENER.open("http://" + api + endpoint + "?itemsPerPage=1000", timeout=3) as response:
        return json.load(response).get("items") or []


def write_atomic(path, data, mode=0o644):
    path.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=".uxduck-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as file:
            file.write(data)
            file.flush()
            os.fsync(file.fileno())
        os.chmod(name, mode)
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def check_name(name):
    """A path's name becomes part of an environment variable's name."""
    if not isinstance(name, str) or not name:
        raise ValueError("A camera path needs a name.")
    if any(c.isupper() for c in name):
        raise ValueError("MediaMTX reads path names from the environment in lower case, so UXDuck cannot set " + name + ".")
    if "_" in name:
        raise ValueError("MediaMTX ends a path's name at the first _ in the environment, so UXDuck cannot set " + name + ".")
    if not re.fullmatch(r"[a-z0-9]+", name):
        raise ValueError("systemd allows only letters, digits and _ in an environment variable's name, so UXDuck cannot set " + name + ".")


def check_picture(settings, sensor):
    """The same rules as UXDuck's own check, made again here before anything is written."""
    if not isinstance(settings, dict):
        raise ValueError("The picture settings are missing.")
    numbers = {}
    for key in ("width", "height", "fps", "idrPeriod", "bitrate"):
        value = settings.get(key)
        if not isinstance(value, int) or isinstance(value, bool):
            raise ValueError("The picture's " + key + " must be a whole number.")
        numbers[key] = value
    mode = settings.get("mode", "")
    if not isinstance(mode, str):
        raise ValueError("The sensor mode must be text.")
    width, height, fps = numbers["width"], numbers["height"], numbers["fps"]
    if not (64 <= width <= 8192 and 64 <= height <= 8192):
        raise ValueError("Use a width and height from 64 to 8192.")
    if width % 2 or height % 2:
        raise ValueError("Use an even width and height.")
    if not 1 <= fps <= 120:
        raise ValueError("Use 1 to 120 pictures a second.")
    if not 1 <= numbers["idrPeriod"] <= 600:
        raise ValueError("Use a keyframe every 1 to 600 pictures.")
    if not 100_000 <= numbers["bitrate"] <= 25_000_000:
        raise ValueError("Use a bitrate from 0.1 to 25 Mbit/s.")
    chosen = None
    if mode:
        match = MODE.fullmatch(mode)
        if not match:
            raise ValueError(mode + " is not a sensor mode. Write it as width:height:bit depth:packing, such as 2304:1296:10:P.")
        chosen = (int(match.group(1)), int(match.group(2)), match.group(3) and int(match.group(3)))
    modes = SENSORS.get(sensor or "")
    if not modes:
        return {**numbers, "mode": mode}
    if chosen:
        found = next((m for m in modes if m[:2] == chosen[:2]), None)
        if not found:
            raise ValueError("The " + sensor + " sensor has no " + str(chosen[0]) + "×" + str(chosen[1]) + " mode.")
        if chosen[2] not in (None, 10):
            raise ValueError("The " + sensor + " sensor's modes are 10-bit.")
        if width > found[0] or height > found[1]:
            raise ValueError(str(width) + "×" + str(height) + " is larger than the sensor mode.")
        if fps > found[2]:
            raise ValueError("That sensor mode gives at most " + str(found[2]) + " pictures a second.")
    elif not any(w >= width and h >= height and f >= fps for w, h, f in modes):
        raise ValueError("The " + sensor + " sensor cannot give " + str(width) + "×" + str(height) + " at " + str(fps) + " pictures a second.")
    return {**numbers, "mode": mode}


def services():
    """Every loaded service with its command lines, user and environment."""
    names = [line.split()[0] for line in run("systemctl", "list-units", "--type=service", "--all", "--no-legend", "--plain").splitlines() if line.strip()]
    if not names:
        return []
    text = run("systemctl", "show", "-p", "Id", "-p", "ExecStart", "-p", "User", "-p", "WorkingDirectory", "-p", "Environment", "--", *names)
    found = []
    for block in text.strip().split("\n\n"):
        props = {}
        for line in block.splitlines():
            key, _, value = line.partition("=")
            props[key] = value
        argvs = [argv.split() for argv in re.findall(r"argv\[\]=(.*?) ; ", props.get("ExecStart", ""))]
        found.append({"id": props.get("Id", ""), "argv": argvs, "user": props.get("User", ""),
                      "directory": props.get("WorkingDirectory", ""), "environment": props.get("Environment", "").split()})
    return found


def mediamtx():
    """The service running a mediamtx binary, and its command line."""
    for service in services():
        for argv in service["argv"]:
            if argv and os.path.basename(argv[0]) == "mediamtx":
                return service, argv
    return None, None


def config_of(service, argv):
    given = [a for a in argv[1:] if not a.startswith("-")]
    if given:
        return given[0]
    directory = pathlib.Path(service["directory"] or "/")
    for candidate in (directory / "mediamtx.yml", pathlib.Path("/usr/local/etc/mediamtx.yml"),
                      pathlib.Path("/usr/etc/mediamtx.yml"), pathlib.Path("/etc/mediamtx/mediamtx.yml")):
        if candidate.exists():
            return str(candidate)
    return None


def api_of(config, environment):
    """MediaMTX's API address, from its YAML's top level and the unit's environment."""
    values = {"api": "no", "apiAddress": ":9997"}
    try:
        text = pathlib.Path(config).read_text() if config else ""
    except OSError:
        text = ""
    for line in text.splitlines():
        match = re.match(r"^(api|apiAddress):\s*([^#]*?)\s*(#.*)?$", line)
        if match:
            values[match.group(1)] = match.group(2).strip("'\"")
    for assignment in environment:
        key, _, value = assignment.partition("=")
        if key == "MTX_API":
            values["api"] = value
        if key == "MTX_APIADDRESS":
            values["apiAddress"] = value
    if values["api"].lower() not in ("yes", "true"):
        return None
    host, _, port = values["apiAddress"].rpartition(":")
    host = host.strip("[]")
    if host in ("", "0.0.0.0", "::"):
        host = "127.0.0.1"
    if not port.isdigit() or not re.fullmatch(r"[0-9A-Za-z.:-]+", host):
        return None
    return ("[" + host + "]" if ":" in host else host) + ":" + port


def sensor_model():
    """The sensor model the kernel names, read passively: the cameras are never opened. With more than one model UXDuck cannot tell which camera is which."""
    models = set()
    for root in SENSOR_NAMES:
        for name_file in sorted(root.glob("*/name")):
            try:
                word = name_file.read_text().split()[0].lower()
            except (OSError, IndexError):
                continue
            if re.fullmatch(r"(imx|ov|ar)\d+[a-z0-9_]*", word):
                models.add(re.sub(r"_(wide|noir)", "", word))
    return models.pop() if len(models) == 1 else None


def system_uid_max():
    try:
        for line in LOGIN_DEFS.read_text().splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[0] == "SYS_UID_MAX" and parts[1].isdigit():
                return int(parts[1])
    except OSError:
        pass
    return 999


def remove_ipc():
    """logind's RemoveIPC, yes unless its configuration says no."""
    files = [LOGIND] if LOGIND.exists() else []
    drop_ins = {}
    for directory in LOGIND_DIRS:
        for path in directory.glob("*.conf"):
            drop_ins[path.name] = path
    files += [drop_ins[name] for name in sorted(drop_ins)]
    value = True
    for path in files:
        try:
            lines = path.read_text().splitlines()
        except OSError:
            continue
        for line in lines:
            match = re.match(r"^\s*RemoveIPC\s*=\s*(\S+)", line)
            if match:
                value = match.group(1).lower() in ("yes", "true", "1", "on")
    return value


def linger_of(user):
    """A login user's sessions ending can stop the cameras (C7). Root and system users are never affected."""
    if not user or user == "root":
        return None
    try:
        entry = pwd.getpwuid(int(user)) if user.isdigit() else pwd.getpwnam(user)
    except (KeyError, ValueError):
        return None
    if entry.pw_uid <= system_uid_max():
        return None
    return {"user": entry.pw_name, "lingering": (LINGER / entry.pw_name).exists(), "removeIpc": remove_ipc()}


def active(unit):
    result = subprocess.run(["systemctl", "is-active", "--", unit], capture_output=True, text=True, timeout=10)
    return result.stdout.strip() == "active"


def watchdog_status():
    installed = WATCHDOG_SCRIPT.exists() and (SYSTEMD / WATCHDOG_UNIT).exists()
    status = {"installed": installed, "running": installed and active(WATCHDOG_UNIT), "gaveUp": False, "restarts": []}
    try:
        state = json.loads(WATCHDOG_STATE.read_text())
    except (OSError, ValueError):
        return status
    status["gaveUp"] = bool(state.get("gaveUp"))
    for restart in state.get("restarts", [])[-10:]:
        if isinstance(restart, dict) and isinstance(restart.get("at"), (int, float)) and isinstance(restart.get("path"), str):
            status["restarts"].append({"atUs": int(restart["at"] * 1_000_000), "path": restart["path"]})
    return status


def drop_in_of(unit):
    return SYSTEMD / (unit + ".d") / DROP_IN


def drop_in_values(path):
    """The settings UXDuck's drop-in holds, by path name and MediaMTX field."""
    values = {}
    try:
        lines = path.read_text().splitlines()
    except OSError:
        return values
    for line in lines:
        match = ENVIRONMENT_LINE.match(line)
        if match:
            values[(match.group(1).lower(), match.group(2))] = match.group(3)
    return values


def render(values):
    lines = ["# Written by UXDuck (ADR-0060): camera settings as MediaMTX's own environment overrides.",
             "# They override the same fields in MediaMTX's YAML. Delete this file and restart the unit to undo.",
             "[Service]"]
    for (name, field), value in sorted(values.items()):
        lines.append('Environment="MTX_PATHS_' + name.upper() + "_" + field + "=" + value + '"')
    return "\n".join(lines) + "\n"


def source_kind(source):
    """What a path takes its pictures from, never the address itself, which can hold a password."""
    if source in ("rpiCamera", "publisher", "redirect"):
        return source
    scheme = source.split("://", 1)[0] if "://" in source else ""
    return scheme if re.fullmatch(r"[a-z][a-z0-9+.-]*", scheme) else "other"


def inbound(item):
    value = item.get("inboundBytes", item.get("bytesReceived", 0))
    return value if isinstance(value, int) else 0


def is_ready(item):
    return bool(item.get("ready", item.get("available", False)))


def read():
    status = {"paths": [], "watchdog": watchdog_status()}
    service, argv = mediamtx()
    if not service:
        status["problem"] = "No MediaMTX service was found on this computer. UXDuck looks for a systemd service that runs a mediamtx binary."
        return status
    config = config_of(service, argv)
    status.update(unit=service["id"], dropIn=str(drop_in_of(service["id"])))
    if config:
        status["config"] = config
    linger = linger_of(service["user"])
    if linger:
        status["linger"] = linger
    api = api_of(config, service["environment"])
    if not api:
        status["problem"] = "MediaMTX's API is off. Turn it on with api: yes in " + (config or "its configuration") + " to see its cameras here."
        return status
    status["api"] = api
    try:
        configured = fetch(api, "/v3/config/paths/list")
        live = {item.get("name"): item for item in fetch(api, "/v3/paths/list")}
    except (OSError, ValueError) as error:
        status["problem"] = "MediaMTX's API at " + api + " did not answer (" + str(getattr(error, "reason", error))[:200] + ")."
        return status
    sensor = sensor_model()
    ours = {name for name, _ in drop_in_values(drop_in_of(service["id"]))}
    for path in configured:
        name = path.get("name")
        if not isinstance(name, str) or name.startswith("~") or name in ("all", "all_others"):
            continue
        item = live.get(name) or {}
        entry = {"name": name, "source": source_kind(str(path.get("source", ""))),
                 "onDemand": bool(path.get("sourceOnDemand")), "ready": is_ready(item),
                 "readers": len(item.get("readers") or []), "bytes": inbound(item)}
        if entry["source"] == "rpiCamera":
            entry["cameraId"] = int(path.get("rpiCameraCamID") or 0)
            entry["settings"] = {"width": int(path.get("rpiCameraWidth") or 0), "height": int(path.get("rpiCameraHeight") or 0),
                                 "fps": int(round(float(path.get("rpiCameraFPS") or 0))), "mode": str(path.get("rpiCameraMode") or ""),
                                 "idrPeriod": int(path.get("rpiCameraIDRPeriod") or 0), "bitrate": int(path.get("rpiCameraBitrate") or 0)}
            entry["setByUxduck"] = name in ours
            if sensor:
                entry["sensor"] = sensor
        status["paths"].append(entry)
    recordings(status)
    sound(status)
    return status


def camera_tables(text):
    """Each [cameras.<path>] table of companion.toml: its record_path, its
    record mode and microphone, the line of its header and the lines of its
    record and microphone keys by key."""
    tables, current = {}, None
    for number, line in enumerate(text.splitlines()):
        table = CAMERAS_TABLE.match(line)
        if table:
            current = tables.setdefault(table.group(1) or table.group(2), {"header": number})
            continue
        if ANY_TABLE.match(line):
            current = None
            continue
        key = STRING_KEY.match(line) if current is not None else None
        if key and key.group(1) == "record_path":
            current["recordPath"] = key.group(2)
        elif key and key.group(1) == "record":
            current["mode"] = key.group(2)
            current.setdefault("lines", {})["record"] = number
        elif key and key.group(1) == "microphone":
            current["microphone"] = key.group(2)
            current.setdefault("lines", {})["microphone"] = number
    return tables


def recordings(status):
    """Attaches each camera's recording on the vehicle to its path, and takes
    the recorded paths out of the list: they are the same camera's second,
    fixed-quality stream, which UXDuck does not adjust (ADR-0066 R1)."""
    try:
        text = COMPANION_TOML.read_text()
    except FileNotFoundError:
        return
    except (OSError, UnicodeDecodeError) as error:
        status["recordProblem"] = "UXDuck could not read " + str(COMPANION_TOML) + " (" + str(error)[:200] + ")."
        return
    by_name = {p["name"]: p for p in status["paths"]}
    recorded, problems = set(), []
    for name, table in camera_tables(text).items():
        path, mode = table.get("recordPath"), table.get("mode", "armed")
        if not path or name not in by_name:
            continue
        if mode not in RECORD_MODES:
            problems.append("[cameras." + name + '] has record = "' + mode[:40] + '", which is not armed, always or off.')
            continue
        record = {"path": path, "mode": mode}
        if by_name.get(path, {}).get("settings"):
            record["settings"] = by_name[path]["settings"]
        by_name[name]["record"] = record
        recorded.add(path)
    status["paths"] = [p for p in status["paths"] if p["name"] not in recorded]
    if problems:
        status["recordProblem"] = " ".join(problems)


def microphones(asound=None, sound_class=None):
    """The sound capture devices in card order (ADR-0067 M2): cards with a
    pcm<N>c folder, known by their USB bus path or else their ALSA id. The
    same rules as uxduck companion's, tested on the same fake trees."""
    asound, sound_class = asound or ASOUND, sound_class or SOUND_CLASS
    try:
        lines = (asound / "cards").read_text().splitlines()
    except FileNotFoundError:
        return []
    found = []
    for line in lines:
        match = CARD_LINE.match(line)
        if not match:
            continue
        number, folder = match.group(1), asound / ("card" + match.group(1))
        captures = sorted(int(m.group(1)) for m in (CAPTURE.fullmatch(p.name) for p in folder.glob("pcm*c")) if m)
        if not captures:
            continue
        try:
            card = (folder / "id").read_text().strip() or match.group(2).strip()
        except OSError:
            card = match.group(2).strip()
        device = os.path.basename(os.path.realpath(sound_class / ("card" + number) / "device"))
        usb = USB_INTERFACE.fullmatch(device)
        found.append({"id": "usb:" + usb.group(1) if usb else "card:" + card, "name": match.group(3) or card,
                      "card": card, "usb": bool(usb), "alsa": "hw:CARD=" + card + ",DEV=" + str(captures[0])})
    return found


def automatic(devices):
    """The device Automatic chooses: the only one, else the first USB one, else none (ADR-0067 M3)."""
    if len(devices) == 1:
        return devices[0]["id"]
    return next((d["id"] for d in devices if d["usb"]), None)


def sound(status):
    """Lists the microphones, attaches each camera's choice and the device it
    gives now, and says when a camera wants sound and GStreamer is missing.
    A file that does not read is said by recordings."""
    try:
        tables = camera_tables(COMPANION_TOML.read_text())
    except (OSError, UnicodeDecodeError):
        tables = {}
    devices = microphones()
    status["microphones"] = [{key: d[key] for key in ("id", "name", "card", "usb")} for d in devices]
    ids = {d["id"] for d in devices}
    wanting = []
    for path in status["paths"]:
        table = tables.get(path["name"])
        if table is None:
            continue
        choice = table.get("microphone", "auto")
        device = automatic(devices) if choice == "auto" else choice if choice in ids else None
        path["microphone"] = {"choice": choice, **({"device": device} if device else {})}
        # A camera on Automatic with no microphone plugged in needs nothing new (ADR-0067).
        if choice != "none" and (device or choice != "auto"):
            wanting.append(path["name"])
    if wanting and not shutil.which(GSTREAMER):
        status["soundProblem"] = ("Sound needs GStreamer on this computer, and " + GSTREAMER + " is not installed. Install " +
                                  GSTREAMER_PACKAGES + ".")


def write_table_key(table, key, value, saved):
    """Writes one key of one [cameras.<path>] table, adding it under the
    header when missing, keeps a copy of the file as it was beside it, keeps
    every other line and the file's mode, and has uxduck companion read the
    file again. saved names the change for the words when the reload fails."""
    text = COMPANION_TOML.read_text()
    lines = text.splitlines(keepends=True)
    entry = key + ' = "' + value + '"\n'
    line = table.get("lines", {}).get(key)
    if line is not None:
        lines[line] = entry
    else:
        if not lines[table["header"]].endswith("\n"):
            lines[table["header"]] += "\n"
        lines.insert(table["header"] + 1, entry)
    mode = COMPANION_TOML.stat().st_mode & 0o777
    write_atomic(COMPANION_TOML.with_name(COMPANION_TOML.name + ".bak"), text.encode(), mode=mode)
    write_atomic(COMPANION_TOML, "".join(lines).encode(), mode=mode)
    if active(COMPANION_UNIT):
        try:
            run("systemctl", "reload", "--", COMPANION_UNIT, timeout=30)
        except (RuntimeError, subprocess.TimeoutExpired) as error:
            raise RuntimeError(saved + " in " + str(COMPANION_TOML) + ", but uxduck companion did not read it again (" +
                               str(error)[:200] + "). It takes effect when uxduck companion next starts.") from error


def set_record(change, found):
    """Writes one camera's record mode into companion.toml, keeping every
    other line as it was, and has uxduck companion read the file again."""
    if not isinstance(change, dict):
        raise ValueError("The recording change is missing.")
    name, mode = change.get("path"), change.get("mode")
    if mode not in RECORD_MODES:
        raise ValueError("Record on the vehicle is armed, always or off.")
    if not any(p["name"] == name and p.get("record") for p in found["paths"]):
        raise ValueError("This computer does not record " + str(name)[:80] + " on the vehicle. Set up recording on the vehicle first.")
    table = camera_tables(COMPANION_TOML.read_text()).get(name)
    if not table or not table.get("recordPath"):
        raise ValueError("[cameras." + str(name)[:80] + "] in " + str(COMPANION_TOML) + " has no record_path, so it does not record on the vehicle. Read its cameras again.")
    write_table_key(table, "record", mode, "Record on the vehicle is saved as " + mode)


def set_microphone(change, found):
    """Writes one camera's microphone into companion.toml (ADR-0067 M3), as
    set_record writes its mode. A device that is not plugged in now can be
    chosen by its id, and gives no sound until it is."""
    if not isinstance(change, dict):
        raise ValueError("The microphone change is missing.")
    name, choice = change.get("path"), change.get("choice")
    if not any(p["name"] == name and p.get("microphone") for p in found["paths"]):
        raise ValueError("uxduck companion does not send " + str(name)[:80] + " on this computer, so it has no microphone to choose. Read its cameras again.")
    known = {m["id"] for m in found.get("microphones", [])}
    if not isinstance(choice, str) or not (choice in ("auto", "none") or choice in known or MICROPHONE_ID.fullmatch(choice)):
        raise ValueError(str(choice)[:80] + " is not a microphone on this computer. Choose auto, none, a microphone it lists, or one not plugged in now by its id, such as usb:1-1.2 or card:Device.")
    table = camera_tables(COMPANION_TOML.read_text()).get(name)
    if not table:
        raise ValueError(str(COMPANION_TOML) + " has no [cameras." + str(name)[:80] + "] table. Read its cameras again.")
    write_table_key(table, "microphone", choice, "The microphone for " + name + " is saved as " + choice)


def own_source():
    """This adapter's text, for the job it hands to systemd. Over SSH it arrives as python3 -c."""
    try:
        return pathlib.Path(__file__).read_text()
    except NameError:
        return sys.orig_argv[sys.orig_argv.index("-c") + 1]


def apply(change, found):
    if not isinstance(change, dict):
        raise ValueError("The picture change is missing.")
    if not found.get("unit") or not found.get("api"):
        raise ValueError(found.get("problem") or "MediaMTX was not found.")
    name = change.get("path")
    check_name(name)
    path = next((p for p in found["paths"] if p["name"] == name), None)
    if not path:
        raise ValueError("MediaMTX has no camera path " + name + ".")
    if path["source"] != "rpiCamera":
        raise ValueError("Only a Raspberry Pi camera (source rpiCamera) can be adjusted here.")
    settings = check_picture(change.get("settings"), path.get("sensor"))
    linger = found.get("linger")
    if linger and linger["removeIpc"] and not linger["lingering"] and change.get("despiteLinger") is not True:
        raise ValueError("Signing in over SSH can stop these cameras. Turn lingering on, or dismiss the warning, before changing them.")
    drop_in = drop_in_of(found["unit"])
    values = drop_in_values(drop_in)
    for key, field in FIELDS:
        values[(name, field)] = str(settings[key])
    plan = {"unit": found["unit"], "dropIn": str(drop_in), "content": render(values), "api": found["api"], "target": name,
            # An on-demand camera has no pictures until something reads it, so it is not waited for.
            "paths": [p["name"] for p in found["paths"] if p["source"] == "rpiCamera" and not p["onDemand"]]}
    return hand_over(plan)


def hand_over(plan):
    """Runs the change in a transient unit and waits for what it did."""
    WORK.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(WORK, 0o700)
    write_atomic(WORK / "job.py", own_source().encode(), mode=0o600)
    write_atomic(WORK / "plan.json", json.dumps(plan).encode(), mode=0o600)
    result = WORK / "result.json"
    result.unlink(missing_ok=True)
    started = subprocess.run(["systemd-run", "--unit", CHANGE_UNIT, "--collect", "--wait", "--quiet",
                              sys.executable, str(WORK / "job.py"), "--job", str(WORK)],
                             capture_output=True, text=True, timeout=170)
    if not result.exists():
        if "already" in started.stderr:
            raise RuntimeError("A camera change is still running on this computer. Read its cameras again in a minute.")
        raise RuntimeError("The camera change did not finish (" + (started.stderr.strip()[-200:] or "no result") + "). Read the cameras again to see where it stopped.")
    return json.loads(result.read_text())


def restart(unit):
    run("systemctl", "daemon-reload", timeout=60)
    run("systemctl", "restart", "--", unit, timeout=120)


def wait_ready(api, names, fallback):
    """None once MediaMTX answers and every camera in names is ready with its received bytes growing, else the first that is not."""
    deadline = time.monotonic() + READY_WAIT_S
    first = {}
    pending = list(names)
    answered = False
    while True:
        try:
            live = {item.get("name"): item for item in fetch(api, "/v3/paths/list")}
            answered = True
        except (OSError, ValueError):
            live = {}
        for name in list(pending):
            item = live.get(name)
            if not item or not is_ready(item):
                continue
            if name not in first:
                first[name] = inbound(item)
            elif inbound(item) > first[name]:
                pending.remove(name)
        if answered and not pending:
            return None
        if time.monotonic() >= deadline:
            return pending[0] if pending else fallback
        time.sleep(POLL_S)


def journal(unit, name, since):
    """MediaMTX's last lines for one camera since the change began, else its last lines."""
    try:
        text = run("journalctl", "-u", unit, "--since", "@" + str(int(since)), "-o", "cat", "--no-pager", "-n", "300", timeout=15)
    except (OSError, RuntimeError, subprocess.TimeoutExpired):
        return []
    # An address another path pulls from can hold a password: keep only its scheme and host.
    lines = [re.sub(r"([a-zA-Z][a-zA-Z0-9+.-]*://)[^/@\s]*@", r"\1", line)[:300] for line in text.splitlines() if line.strip()]
    mine = [line for line in lines if "[path " + name + "]" in line]
    return (mine or lines)[-12:]


def job(work):
    """In the transient unit: keep the current drop-in, write the new one, restart, wait, and roll back if a camera does not come back."""
    plan = json.loads((work / "plan.json").read_text())
    drop_in = pathlib.Path(plan["dropIn"])
    previous = drop_in.read_bytes() if drop_in.exists() else None
    if previous is not None:
        write_atomic(work / "previous.conf", previous, mode=0o600)
    since = time.time()
    write_atomic(drop_in, plan["content"].encode())
    try:
        restart(plan["unit"])
        failed = wait_ready(plan["api"], plan["paths"], plan["target"])
    except (OSError, RuntimeError, subprocess.TimeoutExpired):
        failed = plan["target"]
    result = {"path": plan["target"], "outcome": "applied"}
    if failed:
        result.update(failed=failed, log=journal(plan["unit"], failed, since))
        if previous is None:
            drop_in.unlink(missing_ok=True)
        else:
            write_atomic(drop_in, previous)
        try:
            restart(plan["unit"])
            recovered = wait_ready(plan["api"], plan["paths"], plan["target"]) is None
        except (OSError, RuntimeError, subprocess.TimeoutExpired):
            recovered = False
        result["outcome"] = "rolled-back" if recovered else "rollback-failed"
    write_atomic(work / "result.json", json.dumps(result).encode(), mode=0o600)


def enable_linger(found):
    linger = found.get("linger")
    if not linger:
        raise ValueError("MediaMTX does not run as a login user here, so lingering does not matter.")
    if not re.fullmatch(r"[a-z_][a-z0-9_-]*\$?", linger["user"]):
        raise ValueError("The service's user name is not one UXDuck passes to loginctl.")
    run("loginctl", "enable-linger", "--", linger["user"])


def install_watchdog(found, script):
    unit, api = found.get("unit"), found.get("api")
    if not unit or not api:
        raise ValueError(found.get("problem") or "MediaMTX was not found.")
    if not isinstance(script, str) or "UXDuck's camera watchdog" not in script:
        raise ValueError("The watchdog script is missing.")
    if not re.fullmatch(r"[A-Za-z0-9@._:-]+\.service", unit) or not re.fullmatch(r"[0-9A-Za-z.:\[\]-]+", api):
        raise ValueError("UXDuck cannot write a watchdog for this unit name or API address.")
    write_atomic(WATCHDOG_SCRIPT, script.encode(), mode=0o755)
    write_atomic(SYSTEMD / WATCHDOG_UNIT, (
        "# Written by UXDuck (ADR-0060 C9). Remove it from the computer's Cameras section in UXDuck.\n"
        "[Unit]\nDescription=UXDuck camera watchdog for " + unit + "\nAfter=" + unit + "\n\n"
        "[Service]\nExecStart=" + sys.executable + " " + str(WATCHDOG_SCRIPT) + " --unit " + unit + " --api " + api + "\n"
        "Restart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n").encode())
    run("systemctl", "daemon-reload", timeout=60)
    run("systemctl", "enable", "--", WATCHDOG_UNIT)
    run("systemctl", "restart", "--", WATCHDOG_UNIT)


def remove_watchdog():
    subprocess.run(["systemctl", "disable", "--now", "--", WATCHDOG_UNIT], capture_output=True, timeout=60)
    for path in (SYSTEMD / WATCHDOG_UNIT, WATCHDOG_SCRIPT, WATCHDOG_STATE):
        path.unlink(missing_ok=True)
    run("systemctl", "daemon-reload", timeout=60)

# Recordings on the vehicle (S429, ADR-0066 R12): MediaMTX's time fields in
# recordPath, as regular expressions over a segment's name.
RECORD_FIELDS = {"%Y": r"(?P<Y>\d{4})", "%m": r"(?P<m>\d{2})", "%d": r"(?P<d>\d{2})", "%H": r"(?P<H>\d{2})",
                 "%M": r"(?P<M>\d{2})", "%S": r"(?P<S>\d{2})", "%f": r"(?P<f>\d{6})", "%s": r"(?P<s>\d+)"}
# Two segments are one recording when the second starts within this long of
# the first's end: MediaMTX starts the next segment at the first picture after
# the last, so a recording's segments meet, and one stopped and started again
# has a gap of at least the time it was off.
SAME_RECORDING_S = 2.0
# A segment written to in this long may still be growing: MediaMTX flushes a
# part to the file every recordPartDuration, 1 s by default.
STILL_WRITING_S = 10.0


def get(api, endpoint):
    """One object from MediaMTX's local API."""
    with OPENER.open("http://" + api + endpoint, timeout=3) as response:
        return json.load(response)


def record_store(name, conf):
    """Where a path's segments are, the pattern of their names, and their extension."""
    pattern = conf.get("recordPath") or ""
    own = name + "'s recordPath " + pattern + " must start with an absolute folder that has %path in it, such as /var/lib/uxduck/recordings/%path/%Y-%m-%d_%H-%M-%S-%f."
    if not pattern.startswith("/") or "%path" not in pattern:
        raise ValueError(own)
    parts = pattern.replace("%path", name).split("/")
    fixed = parts
    for index, part in enumerate(parts):
        if "%" in part:
            fixed = parts[:index]
            break
    folder = "/".join(fixed)
    rest = "/".join(parts[len(fixed):])
    if not rest or ".." in fixed:
        raise ValueError(own)
    regex = ""
    for token in re.split(r"(%[A-Za-z])", rest):
        regex += RECORD_FIELDS.get(token, r"[^/]*" if token.startswith("%") and len(token) == 2 else re.escape(token))
    extension = {"fmp4": "mp4", "mpegts": "ts"}.get(conf.get("recordFormat") or "fmp4")
    if not extension:
        raise ValueError(name + "'s recordFormat " + str(conf.get("recordFormat")) + " is not fmp4 or mpegts.")
    return pathlib.Path(folder), re.compile(regex + r"\." + extension + "$"), extension


def name_time(match):
    """When a segment started, by its name, in the computer's local time as MediaMTX writes it."""
    fields = match.groupdict()
    if fields.get("s"):
        return float(fields["s"])
    if not all(fields.get(k) for k in ("Y", "m", "d", "H", "M", "S")):
        return None
    local = time.mktime((int(fields["Y"]), int(fields["m"]), int(fields["d"]), int(fields["H"]), int(fields["M"]), int(fields["S"]), 0, 0, -1))
    return local + int(fields.get("f") or 0) / 1e6


def boxes(data):
    at = 0
    while at + 8 <= len(data):
        size = int.from_bytes(data[at:at + 4], "big")
        if size < 8 or at + size > len(data):
            return
        yield data[at + 4:at + 8], data[at + 8:at + size]
        at += size


def mp4_seconds(path):
    """How long a fragmented MP4's whole fragments play, by its video track, read
    from the moov and each moof alone. None when it has no video fragment."""
    scales, video, start, end = {}, None, None, None
    with open(path, "rb") as file:
        while True:
            header = file.read(8)
            if len(header) < 8:
                break
            size, kind = int.from_bytes(header[:4], "big"), header[4:]
            head = 8
            if size == 1:
                size, head = int.from_bytes(file.read(8), "big"), 16
            if size < head:
                break
            if kind not in (b"moov", b"moof"):
                file.seek(size - head, 1)
                continue
            body = file.read(size - head)
            if len(body) < size - head:
                break
            if kind == b"moov":
                for trak_kind, trak in boxes(body):
                    if trak_kind != b"trak":
                        continue
                    found = dict(boxes(trak))
                    tkhd, mdia = found.get(b"tkhd", b""), dict(boxes(found.get(b"mdia", b"")))
                    at = 20 if tkhd[:1] == b"\x01" else 12
                    track = int.from_bytes(tkhd[at:at + 4], "big")
                    mdhd, hdlr = mdia.get(b"mdhd", b""), mdia.get(b"hdlr", b"")
                    scales[track] = int.from_bytes(mdhd[20:24] if mdhd[:1] == b"\x01" else mdhd[12:16], "big")
                    if hdlr[8:12] == b"vide" and video is None:
                        video = track
                continue
            for traf_kind, traf in boxes(body):
                if traf_kind != b"traf":
                    continue
                track, default, base, ticks = None, 0, None, 0
                for kind2, child in boxes(traf):
                    flags = int.from_bytes(child[1:4], "big")
                    if kind2 == b"tfhd":
                        track = int.from_bytes(child[4:8], "big")
                        if flags & 0x8:
                            at = 8 + (8 if flags & 0x1 else 0) + (4 if flags & 0x2 else 0)
                            default = int.from_bytes(child[at:at + 4], "big")
                    elif kind2 == b"tfdt":
                        base = int.from_bytes(child[4:12] if child[:1] == b"\x01" else child[4:8], "big")
                    elif kind2 == b"trun":
                        count = int.from_bytes(child[4:8], "big")
                        at = 8 + (4 if flags & 0x1 else 0) + (4 if flags & 0x4 else 0)
                        step = 4 * sum(1 for f in (0x100, 0x200, 0x400, 0x800) if flags & f)
                        if flags & 0x100:
                            ticks += sum(int.from_bytes(child[at + i * step:at + i * step + 4], "big") for i in range(count))
                        else:
                            ticks += count * default
                if track == video and base is not None:
                    start = base if start is None else min(start, base)
                    end = base + ticks if end is None else max(end, base + ticks)
    if video is None or start is None or not scales.get(video):
        return None
    return (end - start) / scales[video]


def path_recordings(name, conf, ready):
    """A record path's segments, grouped into recordings, oldest first."""
    folder, pattern, extension = record_store(name, conf)
    segments, now = [], time.time()
    for root, dirs, files in os.walk(folder):
        dirs.sort()
        for file in files:
            full = pathlib.Path(root) / file
            relative = str(full.relative_to(folder))
            match = pattern.search(relative)
            stat = full.lstat()
            if not match or not os.path.isfile(full) or os.path.islink(full):
                continue
            seconds = mp4_seconds(full) if extension == "mp4" else None
            started = name_time(match)
            if started is None:
                started = stat.st_mtime - (seconds or 0)
            segments.append({"path": str(full), "name": relative, "size": stat.st_size,
                             "start": started, "seconds": seconds, "modified": stat.st_mtime})
    segments.sort(key=lambda s: (s["start"], s["name"]))
    if segments:
        newest = segments[-1]
        newest["writing"] = now - newest["modified"] < STILL_WRITING_S or (conf.get("record") is True and ready)
    recordings = []
    for segment in segments:
        if segment["size"] == 0 and not segment.get("writing"):
            continue
        end = lambda s: s["start"] + s["seconds"] if s["seconds"] is not None else s["modified"]
        if recordings and segment["start"] <= end(recordings[-1][-1]) + SAME_RECORDING_S:
            recordings[-1].append(segment)
        else:
            recordings.append([segment])
    listed = []
    for group in recordings:
        last = group[-1]
        finish = last["start"] + last["seconds"] if last["seconds"] is not None else last["modified"]
        entry = {"id": name + "/" + group[0]["name"], "startedAtUs": int(group[0]["start"] * 1e6),
                 "durationUs": max(0, int((finish - group[0]["start"]) * 1e6)),
                 "segments": [{"path": s["path"], "size": s["size"]} for s in group],
                 "recording": bool(last.get("writing"))}
        if extension != "mp4":
            entry["notDownloadable"] = (name + " is recorded as MPEG-TS, and UXDuck downloads fragmented MP4 recordings. Set recordFormat: fmp4 for " + name + " in MediaMTX's configuration.")
        listed.append(entry)
    return str(folder), listed


def list_recordings(found):
    """Each camera that records on the vehicle, with its recordings."""
    cameras = []
    for path in found["paths"]:
        record = path.get("record")
        if not record:
            continue
        entry = {"camera": path["name"], "path": record["path"], "mode": record["mode"], "recordings": []}
        try:
            name = urllib.parse.quote(record["path"], safe="")
            conf = get(found["api"], "/v3/config/paths/get/" + name)
            try:
                ready = is_ready(get(found["api"], "/v3/paths/get/" + name))
            except OSError:
                ready = False
            entry["folder"], entry["recordings"] = path_recordings(record["path"], conf, ready)
        except PermissionError as error:
            entry["problem"] = "The user UXDuck signs in as cannot read " + str(error.filename) + ". Give it read access to the recordings."
        except (OSError, ValueError) as error:
            entry["problem"] = "UXDuck could not list " + record["path"] + "'s recordings (" + str(getattr(error, "reason", error))[:200] + ")."
        cameras.append(entry)
    return {"cameras": cameras, **({"problem": found["problem"]} if found.get("problem") else {}),
            **({"recordProblem": found["recordProblem"]} if found.get("recordProblem") else {})}


def main():
    request = json.loads(sys.stdin.read() or "{}")
    changes = [key for key in ("apply", "linger", "watchdog", "record", "microphone") if request.get(key)]
    if len(changes) > 1:
        raise ValueError("Make one camera change at a time.")
    found = read()
    if request.get("recordings"):
        if changes:
            raise ValueError("List the recordings on their own.")
        print("recordings=" + json.dumps(list_recordings(found), ensure_ascii=True))
        return
    applied = None
    if request.get("apply"):
        applied = apply(request["apply"], found)
    elif request.get("linger"):
        enable_linger(found)
    elif request.get("watchdog") == "install":
        install_watchdog(found, request.get("watchdogScript"))
    elif request.get("watchdog") == "remove":
        remove_watchdog()
    elif request.get("watchdog"):
        raise ValueError("Install or remove the watchdog.")
    elif request.get("record"):
        set_record(request["record"], found)
    elif request.get("microphone"):
        set_microphone(request["microphone"], found)
    if changes:
        found = read()
    if applied:
        found["applied"] = applied
    print("cameras=" + json.dumps(found, ensure_ascii=True))


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--job":
        job(pathlib.Path(sys.argv[2]))
        sys.exit(0)
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print("uxduck-camera: " + (str(error) if isinstance(error, (ValueError, RuntimeError)) else "Reading or changing the cameras failed (" + type(error).__name__ + ": " + str(error)[:200] + ")."), file=sys.stderr)
        sys.exit(1)
