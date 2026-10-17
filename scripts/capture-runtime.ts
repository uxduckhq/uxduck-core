/** Build the isolated capture runtime from pinned, verified corresponding source. */
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

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
    url: "https://ffmpeg.org/releases/ffmpeg-8.1.2.tar.xz",
    sha: "464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c",
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
        "-verify_arch",
        target.startsWith("aarch64") ? "arm64" : "x86_64",
        executable,
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
        const response = await fetch(source.url);
        if (!response.ok) throw new Error(`Cannot download ${source.url}: ${response.status}`);
        writeFileSync(archive, Buffer.from(await response.arrayBuffer()));
      }
      if (createHash("sha256").update(readFileSync(archive)).digest("hex") !== source.sha)
        throw new Error(`Capture source hash mismatch: remove ${archive} and rebuild.`);
      const dir = join(work, source.dir);
      mkdirSync(dir);
      execFileSync("tar", ["-xf", archive, "--strip-components=1", "-C", dir], {
        stdio: "inherit",
      });
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
      const encoders = execFileSync(executable, ["-hide_banner", "-encoders"], {
        encoding: "utf8",
      });
      if (!encoders.includes("libx264"))
        throw new Error("The capture helper is missing its required H.264 encoder.");

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
    } else {
      execFileSync("lipo", [
        "-verify_arch",
        target.startsWith("aarch64") ? "arm64" : "x86_64",
        executable,
      ]);
      const components = readFileSync(join(work, "ffmpeg/config_components.h"), "utf8");
      for (const required of [
        "AVFOUNDATION_INDEV",
        "LIBX264_ENCODER",
        "MPEGTS_MUXER",
        "BWDIF_FILTER",
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
