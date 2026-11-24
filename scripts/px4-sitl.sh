#!/usr/bin/env bash
# Builds PX4's SITL with its built-in SIH physics for the installed daemon's
# simulation manager (S17, ADR-0011), on macOS or Linux, into
# .cache/simulators/native/px4/: `bin/px4` with its command links, `etc/` with
# the start-up scripts and airframes, and PX4's licence. No Gazebo: SIH flies
# multicopters, planes, VTOLs and rovers inside PX4.
#
# UXDuck's patches (scripts/px4-patches/) are applied first, so a simulation's
# MAVLink answers this computer only, as UXDuck's ArduPilot builds do, its
# timer queue cannot tear and freeze it, and a home altitude change while a
# mission's command item is current leaves the mission's altitude setpoints
# alone (two fixes PX4 made after 1.17.0).
#
# On macOS, PX4_ARCH=arm64 or x86_64 builds for that architecture instead,
# into .cache/simulators/native-<arch>/px4/, which is how the catalogue gets
# its Intel build on an Apple silicon runner. MACOSX_DEPLOYMENT_TARGET sets
# the oldest macOS it runs on.
#
#   scripts/px4-sitl.sh [tag]    (default v1.17.0)
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=scripts/simulator-cache.sh
source "$root/scripts/simulator-cache.sh"
tag="${1:-v1.17.0}"
work="$root/target/px4-sitl"
px4="$work/PX4-Autopilot"
out="$sim_cache/native/px4"
build="build/px4_sitl_default"

if [ "$(uname)" != Darwin ] && [ "$(uname)" != Linux ]; then
  echo "PX4 SITL builds on macOS and Linux only" >&2
  exit 1
fi
arch=${PX4_ARCH:-}
if [ -n "$arch" ]; then
  case "$(uname)-$arch" in
    Darwin-arm64 | Darwin-x86_64) ;;
    *)
      echo "px4-sitl.sh: PX4_ARCH=$arch is for macOS, as arm64 or x86_64" >&2
      exit 1
      ;;
  esac
  out="$sim_cache/native-$arch/px4"
  build="build/px4_sitl_default-$arch"
  # From the environment, so PX4's own CMake projects inside its build,
  # such as the Micro XRCE-DDS client, are built for it too.
  export CMAKE_OSX_ARCHITECTURES=$arch
fi

mkdir -p "$work"
if [ ! -d "$px4/.git" ]; then
  git clone --depth 1 --branch "$tag" https://github.com/PX4/PX4-Autopilot.git "$px4"
fi
cd "$px4"
if [ "$(git describe --tags --exact-match 2>/dev/null)" != "$tag" ]; then
  git checkout --quiet -- .
  git fetch --depth 1 origin tag "$tag"
  git checkout --quiet "$tag"
fi
git submodule update --init --recursive --depth 1 --jobs 8

for patch in "$root"/scripts/px4-patches/*.patch; do
  if git apply --reverse --check "$patch" 2>/dev/null; then
    continue
  fi
  git apply "$patch" || {
    echo "px4-sitl.sh: $(basename "$patch") does not apply to PX4 $tag" >&2
    exit 1
  }
done

python3 -m venv "$work/venv" >/dev/null 2>&1 || true
# shellcheck disable=SC1091
source "$work/venv/bin/activate"
# What the SITL build's code generators import; the rest of PX4's
# requirements are for its tools and other targets.
pip install --quiet 'empy>=3.3,<4' jinja2 jsonschema kconfiglib lark packaging \
  pyros-genmsg pyyaml toml future setuptools numpy psutil

generator=()
if command -v ninja >/dev/null; then generator=(-G Ninja); fi
jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
started=$(date +%s)
cmake -S . -B "$build" "${generator[@]}" -DCONFIG=px4_sitl_default
cmake --build "$build" -j "$jobs"
echo "built PX4 $tag${arch:+ for $arch} in $(($(date +%s) - started)) s"

rm -rf "$out"
mkdir -p "$out"
# Links stay links: every px4-<command> is the one program.
cp -R "$build/bin" "$out/"
cp -R "$build/etc" "$out/"
cp LICENSE "$out/LICENSE"
echo "${tag#v}" >"$out/version.txt"
du -sh "$out"
