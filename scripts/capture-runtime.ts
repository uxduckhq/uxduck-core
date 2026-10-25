/** Build the isolated capture runtime from pinned, verified corresponding source. */
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { join, relative, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const native =
  process.platform === "darwin"
    ? `${process.arch === "arm64" ? "aarch64" : "x86_64"}-apple-darwin`
    : process.platform === "win32"
      ? "x86_64-pc-windows-msvc"
      : `${process.arch === "arm64" ? "aarch64" : "x86_64"}-unknown-linux-gnu`;
const target = process.argv[2] ?? native;
if (!/^(aarch64|x86_64)-(apple-darwin|unknown-linux-gnu|pc-windows-msvc)$/.test(target))
  throw new Error(`Unsupported capture target ${target}`);
if (target !== native && !(process.platform === "darwin" && target.endsWith("apple-darwin")))
  throw new Error(`Build ${target} on its own OS/architecture (${native} here).`);
const canRun = target === native;
const base = join(root, "target/capture", target);
const runtime = join(base, "capture");
const sources = [
  {
    name: "ffmpeg.tar.xz",
    url: "https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz",
    sha: "8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e",
    dir: "ffmpeg",
  },
  {
    name: "x264.tar.gz",
    url: "https://codeload.github.com/mirror/x264/tar.gz/31e19f92f00c7003fa115047ce50978bc98c3a0d",
    sha: "d053c9d86988d6bc78237ca5205865c5ddf99c98ef4cd9927eec8f6d388f6dd9",
    dir: "x264",
  },
];
const recipes = [
  "tools/capture-runtime/build.sh",
  "tools/capture-runtime/patch-avfoundation.py",
  "tools/capture-runtime/test-frame-rate.py",
];
const digest = createHash("sha256").update(JSON.stringify(sources)).update(target);
for (const file of recipes) digest.update(readFileSync(join(root, file)));
const version = digest.digest("hex");
const executable = join(runtime, process.platform === "win32" ? "ffmpeg.exe" : "ffmpeg");
const receipt = join(runtime, "build.json");
mkdirSync(base, { recursive: true });
const lock = join(base, "build.lock");
try {
  mkdirSync(lock);
} catch {
  throw new Error(
    `A capture runtime build is already running. Wait for it to finish; after an interrupted build remove ${lock}.`,
  );
}
try {
  if (
    existsSync(receipt) &&
    existsSync(executable) &&
    [
      "FFmpeg-LICENSE",
      "x264-LICENSE",
      ...recipes.map((file) => `sources/${file.split("/").at(-1)!}`),
      ...sources.map((s) => `sources/${s.name}`),
    ].every((file) => existsSync(join(runtime, file))) &&
    JSON.parse(readFileSync(receipt, "utf8")).recipe === version &&
    createHash("sha256").update(readFileSync(executable)).digest("hex") ===
      JSON.parse(readFileSync(receipt, "utf8")).sha256
  ) {
    if (canRun) execFileSync(executable, ["-hide_banner", "-version"], { stdio: "ignore" });
    else
      execFileSync("lipo", [
        executable,
        "-verify_arch",
        target.startsWith("aarch64") ? "arm64" : "x86_64",
      ]);
    console.log(`Capture runtime ready: ${runtime}`);
  } else {
    const work = join(base, "build");
    rmSync(work, { recursive: true, force: true });
    rmSync(runtime, { recursive: true, force: true });
    mkdirSync(work, { recursive: true });
    mkdirSync(join(runtime, "sources"), { recursive: true });
    for (const source of sources) {
      const archive = join(base, source.name);
      if (!existsSync(archive)) {
        // curl, not fetch: ffmpeg.org can take longer to connect than fetch's
        // fixed 10 s, and it retries. Forward slashes for MSYS2's curl.
        const partial = `${archive}.part`;
        execFileSync(
          "curl",
          [
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "60",
            "--retry",
            "5",
            "--retry-all-errors",
            "--output",
            partial.replaceAll("\\", "/"),
            source.url,
          ],
          { stdio: "inherit" },
        );
        renameSync(partial, archive);
      }
      if (createHash("sha256").update(readFileSync(archive)).digest("hex") !== source.sha)
        throw new Error(`Capture source hash mismatch: remove ${archive} and rebuild.`);
      const dir = join(work, source.dir);
      mkdirSync(dir);
      // From inside the folder, with a relative POSIX path: MSYS2's GNU tar,
      // which builds this on Windows, misreads `C:` and backslashes.
      execFileSync(
        "tar",
        ["-xf", relative(dir, archive).replaceAll("\\", "/"), "--strip-components=1"],
        { cwd: dir, stdio: "inherit" },
      );
      copyFileSync(archive, join(runtime, "sources", source.name));
    }
    execFileSync(
      process.platform === "win32" ? "python" : "python3",
      [join(root, recipes[1]!), join(work, "ffmpeg")],
      { stdio: "inherit" },
    );
    execFileSync(
      "bash",
      [
        join(root, recipes[0]!).replaceAll("\\", "/"),
        work.replaceAll("\\", "/"),
        runtime.replaceAll("\\", "/"),
        target.split("-")[0]!,
      ],
      { stdio: "inherit" },
    );
    if (canRun) {
      execFileSync(executable, ["-hide_banner", "-version"], { stdio: "inherit" });
      const devices = execFileSync(executable, ["-hide_banner", "-devices"], { encoding: "utf8" });
      const expected =
        process.platform === "darwin"
          ? "avfoundation"
          : process.platform === "win32"
            ? "dshow"
            : "video4linux2";
      if (!devices.includes(expected))
        throw new Error(
          `The built capture helper has no ${expected} device input. Check configure's dependency warnings.`,
        );
      const formats = execFileSync(executable, ["-hide_banner", "-demuxers"], { encoding: "utf8" });
      const protocols = execFileSync(executable, ["-hide_banner", "-protocols"], {
        encoding: "utf8",
      });
      const filters = execFileSync(executable, ["-hide_banner", "-bsfs"], { encoding: "utf8" });
      if (
        !/mov,mp4/.test(formats) ||
        !/mpegts/.test(formats) ||
        !/^\s*file\s*$/m.test(protocols) ||
        !/^h264_mp4toannexb$/m.test(filters)
      )
        throw new Error(
          "The media helper lacks required MP4/MPEG-TS file playback support. Rebuild it with pnpm capture:build.",
        );
      const encoders = execFileSync(executable, ["-hide_banner", "-encoders"], {
        encoding: "utf8",
      });
      if (!encoders.includes("libx264"))
        throw new Error("The capture helper is missing its required H.264 encoder.");
      if (!/^ A\S* aac /m.test(encoders))
        throw new Error("The capture helper is missing its AAC encoder for a camera's sound.");

      if (process.platform === "darwin") {
        execFileSync("python3", [join(root, recipes[2]!), join(work, "ffmpeg")], {
          stdio: "inherit",
        });
        // Exercise the real AVFoundation option parser without opening a camera.
        // Camera default sends zero; VIDEO_RATE rejects it before device lookup.
        const missingCamera = spawnSync(
          executable,
          [
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostdin",
            "-f",
            "avfoundation",
            "-video_device_id",
            "uxduck-build-check-missing-camera",
            "-framerate",
            "0",
            "-i",
            "none:none",
            "-frames:v",
            "1",
            "-c:v",
            "libx264",
            "-f",
            "mpegts",
            "pipe:1",
          ],
          { encoding: "utf8", timeout: 15_000 },
        );
        if (
          missingCamera.error ||
          missingCamera.status === 0 ||
          !missingCamera.stderr.includes("The selected camera is not connected")
        )
          throw new Error(
            `The capture helper cannot select a camera in Camera default mode: ${missingCamera.error?.message ?? missingCamera.stderr}`,
          );
        // A sound input is chosen by its exact identity too (S148).
        const missingInput = spawnSync(
          executable,
          [
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostdin",
            "-f",
            "avfoundation",
            "-audio_device_id",
            "uxduck-build-check-missing-input",
            "-i",
            "none:none",
            "-t",
            "0.1",
            "-c:a",
            "aac",
            "-f",
            "mpegts",
            "pipe:1",
          ],
          { encoding: "utf8", timeout: 15_000 },
        );
        if (
          missingInput.error ||
          missingInput.status === 0 ||
          !missingInput.stderr.includes("The selected sound input is not connected")
        )
          throw new Error(
            `The capture helper cannot select a sound input by its identity: ${missingInput.error?.message ?? missingInput.stderr}`,
          );
      }

      const sample = execFileSync(executable, [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=64x48:rate=25",
        "-frames:v",
        "2",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-f",
        "mpegts",
        "pipe:1",
      ]);
      if (sample.length < 188)
        throw new Error("The capture helper could not encode a test picture.");
      // Sound as a camera's: PCM in, resampled, AAC out in MPEG-TS (S148).
      const sound = execFileSync(executable, [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=44100:duration=0.5",
        "-c:a",
        "aac",
        "-ar",
        "48000",
        "-f",
        "mpegts",
        "pipe:1",
      ]);
      if (sound.length < 188) throw new Error("The capture helper could not encode test sound.");
    } else {
      execFileSync("lipo", [
        executable,
        "-verify_arch",
        target.startsWith("aarch64") ? "arm64" : "x86_64",
      ]);
      const components = readFileSync(join(work, "ffmpeg/config_components.h"), "utf8");
      for (const required of [
        "AVFOUNDATION_INDEV",
        "LIBX264_ENCODER",
        "MPEGTS_MUXER",
        "BWDIF_FILTER",
        "AAC_ENCODER",
        "ARESAMPLE_FILTER",
        "FILE_PROTOCOL",
        "MOV_DEMUXER",
        "MPEGTS_DEMUXER",
        "H264_MP4TOANNEXB_BSF",
      ]) {
        if (!components.includes(`#define CONFIG_${required} 1`))
          throw new Error(`The cross-built capture helper is missing ${required}.`);
      }
    }
    for (const file of recipes)
      copyFileSync(join(root, file), join(runtime, "sources", file.split("/").at(-1)!));
    writeFileSync(
      join(runtime, "sources", "sources.json"),
      JSON.stringify(sources, null, 2) + "\n",
    );
    writeFileSync(
      receipt,
      JSON.stringify({
        recipe: version,
        target,
        sha256: createHash("sha256").update(readFileSync(executable)).digest("hex"),
      }) + "\n",
    );
    console.log(`Capture runtime built: ${runtime}`);
  }
} finally {
  rmSync(lock, { recursive: true, force: true });
}
