# Local camera runtime

Build with `pnpm capture:build` (or `node scripts/capture-runtime.ts <Rust target>`).
Use Xcode command-line tools on macOS, GCC/build-essential on Linux, and MSYS2
MINGW64 on Windows. The build needs make, pkg-config, Python 3, tar and, on x86,
NASM. Both macOS architectures can be built on a Mac; other targets build on
their own OS/architecture. Cross-built Mac binaries are checked by their Mach-O
architecture and compiled capture components; their execution is verified on the
target architecture. No installed codec libraries are used.

The script downloads FFmpeg 8.1.2 and x264 commit
`31e19f92f00c7003fa115047ce50978bc98c3a0d`, validates their pinned SHA-256 hashes,
applies the small AVFoundation identity/default-mode patch, builds a static
standalone executable and checks the required capture input and H.264 encoders.
For a camera's sound (S148) it also has FFmpeg's AAC encoder, PCM decoders and
resampling, selects an AVFoundation sound input by its exact unique ID
(`audio_device_id`, checked with a nonexistent ID as for cameras), and on Linux
reads ALSA, which needs libasound's headers (`libasound2-dev`) to build.
On macOS it also checks that Camera default's zero frame rate reaches exact
device lookup, using a nonexistent ID so no physical camera is opened.
Camera default holds the device configuration lock until the capture session
starts, as explicit modes do, so startup cannot replace the active mode.
The input declares its actual frame rate from CoreMedia timing rather than
letting the encoder mistake the microsecond timestamp clock for one million
frames per second. Native macOS builds exercise the real stream-setup function
with six synthetic CoreMedia timing cases, including PAL, NTSC and missing timing;
run them separately with `python3 tools/capture-runtime/test-frame-rate.py
target/capture/<target>/build/ffmpeg`. No physical camera is needed.
`target/capture/<target>/capture/` is the complete distribution. Required files
are never replaced by an arbitrary FFmpeg found on PATH. Development can select
an explicit compatible helper with `UXDUCK_CAPTURE_FFMPEG`.

The runtime is a separate executable; no GPL library is linked into UXDuck.
Distribute the complete capture directory, including the licenses and `sources/`.
The source archives, patch and build script are the corresponding source. To
rebuild it independently, unpack `ffmpeg.tar.xz` and `x264.tar.gz` into
`<work>/ffmpeg` and `<work>/x264` using `tar --strip-components=1`, run
`python3 patch-avfoundation.py <work>/ffmpeg`, then
`bash build.sh <work> <runtime-output> <aarch64-or-x86_64>`.

Packaged macOS camera access is granted to UXDuck via the app usage description
and the signed child. Verify that permission attribution in a signed install;
a terminal-launched development binary may be attributed to its terminal.
The hardware matrix and pending permission/latency checks are in S139's review.
