#!/usr/bin/env bash
# Builds ArduPilot's native SITL for the installed daemon's simulation
# manager (S17, ADR-0011), for the vehicles given (default: all six programs),
# into .cache/simulators/native/ (Blimp is `blimp`, others `ardu<vehicle>`). It uses the same ArduPilot checkout
# and commit as scripts/simulator.sh (target/wasm-sitl/ardupilot).
#
#   scripts/sitl.sh [copter] [plane] [rover] [sub] [blimp] ...
#
# On macOS, SITL_ARCH=arm64 or x86_64 builds for that architecture instead,
# into .cache/simulators/native-<arch>/, which is how the catalogue gets its
# Intel builds on an Apple silicon runner. MACOSX_DEPLOYMENT_TARGET sets the
# oldest macOS the programs run on; without it they need the build machine's.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=scripts/simulator-cache.sh
source "$root/scripts/simulator-cache.sh"
source "$root/scripts/simulator-programs.sh"
work="$root/target/wasm-sitl"
ap="$work/ardupilot"
out="$sim_cache/native"
build="build-native"
arch=${SITL_ARCH:-}
if [ -n "$arch" ]; then
  case "$(uname)-$arch" in
    Darwin-arm64 | Darwin-x86_64) ;;
    *)
      echo "sitl.sh: SITL_ARCH=$arch is for macOS, as arm64 or x86_64" >&2
      exit 1
      ;;
  esac
  out="$sim_cache/native-$arch"
  build="build-native-$arch"
  export CFLAGS="${CFLAGS:-} -arch $arch" CXXFLAGS="${CXXFLAGS:-} -arch $arch"
  export LDFLAGS="${LDFLAGS:-} -arch $arch"
fi
mkdir -p "$out"

pin=${ARDUPILOT_COMMIT:-$(sed -n 's/.*ARDUPILOT_COMMIT:-\([0-9a-f]\{40\}\).*/\1/p' "$root/scripts/simulator.sh")}
[ -n "$pin" ] || { echo "sitl.sh: required ArduPilot source pin is missing" >&2; exit 1; }
current=$(git -C "$ap" rev-parse HEAD 2>/dev/null || true)
wanted=$(git -C "$ap" rev-parse "$pin^{commit}" 2>/dev/null || true)
if [ ! -x "$ap/waf" ] || [ "$current" != "$wanted" ]; then
  # The WebAssembly script fetches and pins the checkout; build nothing there.
  # A cache can restore its .git without its files, so the files decide.
  "$root/scripts/simulator.sh" --checkout-only
fi
python3 "$root/scripts/prepare-simulator.py" "$ap"

cd "$ap"
# A CI cache can restore part of a venv; one without its activate script is
# made again, and a failure to make it stops here.
if [ ! -f "$work/venv/bin/activate" ]; then
  rm -rf "$work/venv"
  python3 -m venv "$work/venv"
fi
# shellcheck disable=SC1091
source "$work/venv/bin/activate"
pip install --quiet empy==3.3.4 pexpect future lxml
# Xcode's linker refuses ArduPilot's packed version struct ("pointer not
# aligned") with chained fixups, and no longer has -ld_classic.
if [ "$(uname)" = Darwin ]; then export LDFLAGS="${LDFLAGS:-} -Wl,-no_fixup_chains"; fi
./waf configure --board sitl --out "$build" >/dev/null

vehicles=("$@")
[ ${#vehicles[@]} -gt 0 ] || vehicles=("${SIMULATOR_PROGRAMS[@]}")
for vehicle in "${vehicles[@]}"; do
  case "$vehicle" in
    ardu* | blimp) program="$vehicle" ;;
    *) program="ardu$vehicle" ;;
  esac
  started=$(date +%s)
  ./waf build --out "$build" --target "bin/$program"
  cp "$build/sitl/bin/$program" "$out/"
  echo "built $vehicle in $(( $(date +%s) - started )) s"
done
ls -l "$out"
