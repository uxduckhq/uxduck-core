#!/usr/bin/env bash
# Builds ArduPilot's native SITL for the installed daemon's simulation
# manager (S17, ADR-0011), for the vehicles given (default: copter plane),
# into .cache/simulators/native/ (Blimp is `blimp`, others `ardu<vehicle>`). It uses the same ArduPilot checkout
# and commit as scripts/simulator.sh (target/wasm-sitl/ardupilot).
#
#   scripts/sitl.sh [copter] [plane] [rover] [sub] [blimp] ...
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=scripts/simulator-cache.sh
source "$root/scripts/simulator-cache.sh"
work="$root/target/wasm-sitl"
ap="$work/ardupilot"
out="$sim_cache/native"
mkdir -p "$out"

if [ ! -d "$ap/.git" ]; then
  # The WebAssembly script fetches and pins the checkout; build nothing there.
  "$root/scripts/simulator.sh" --checkout-only
fi

cd "$ap"
python3 -m venv "$work/venv" >/dev/null 2>&1 || true
# shellcheck disable=SC1091
source "$work/venv/bin/activate"
pip install --quiet empy==3.3.4 pexpect future lxml
# Xcode's linker refuses ArduPilot's packed version struct ("pointer not
# aligned") with chained fixups, and no longer has -ld_classic.
if [ "$(uname)" = Darwin ]; then export LDFLAGS="${LDFLAGS:-} -Wl,-no_fixup_chains"; fi
./waf configure --board sitl --out build-native >/dev/null

vehicles=("$@")
[ ${#vehicles[@]} -gt 0 ] || vehicles=(copter plane)
for vehicle in "${vehicles[@]}"; do
  program="ardu$vehicle"
  [ "$vehicle" != blimp ] || program=blimp
  started=$(date +%s)
  ./waf build --out build-native --target "bin/$program"
  cp "build-native/sitl/bin/$program" "$out/"
  echo "built $vehicle in $(( $(date +%s) - started )) s"
done
ls -l "$out"
