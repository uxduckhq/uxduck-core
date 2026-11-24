# Fake microphones

Copies of what a Raspberry Pi shows under `/proc/asound` and `/sys/class/sound`, for ADR-0067 M2. The companion's Rust and `camera.py` both read each case and check it against its `expected.json`: the capture devices in card order, and the device Automatic chooses.

Each case keeps its `proc` tree as files, apart from the playback-only `pcm<N>p` folders. Those hold nothing, git keeps no empty folder, and nothing reads them, since only a card's `pcm<N>c` capture folders say it records. Its `sys` tree is built when a test runs, from `devices.json`: each card's device folder under `sys/devices` and the `sys/class/sound/<card>/device` link to it. A USB device's folder, such as `3-1.2:1.0`, has a colon in its name, which Windows cannot check out, so the repository holds none.

- `none`: the Pi 5's two HDMI outputs, which play only.
- `one-usb`: a USB microphone beside them.
- `several`: two USB microphones and the `snd-aloop` loopback card, which is not USB.
- `loopback-only`: the loopback card alone, the cloud test's microphone.

Every test builds its case in a temporary folder, and a hot-plug test adds or removes a card there.
