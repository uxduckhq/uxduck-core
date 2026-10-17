#!/usr/bin/env bash
# Builds ArduPilot's WebAssembly SITL (ArduPilot/ardupilot#34206) for the
# browser's simulated vehicles (S20, ADR-0011), for the vehicles given
# (default: plane copter rover sub blimp; S37), into .cache/simulators/wasm/ardu<vehicle>.{js,wasm}.
# The JavaScript names its .wasm, so the names stay. Emscripten and the
# ArduPilot checkout are kept in target/wasm-sitl/ between runs; apps/web
# serves and ships the cached output (apps/web/scripts/simulator-assets.ts).
#
#   scripts/simulator.sh [plane] [copter] [rover] [sub] [blimp] ...
set -euo pipefail

EMSCRIPTEN_VERSION=6.0.8
# The merge of ArduPilot/ardupilot#34206.
ARDUPILOT_COMMIT=${ARDUPILOT_COMMIT:-d5420ff97c843795e0aaf2ec72a06208f8e8d102}

root=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=scripts/simulator-cache.sh
source "$root/scripts/simulator-cache.sh"
work="$root/target/wasm-sitl"
out="$sim_cache/wasm"

ap="$work/ardupilot"
# Its own repository, not one git cannot read: a CI cache can restore a
# partial .git, and git would then fall through to an enclosing checkout.
if [ "$(git -C "$ap" rev-parse --git-dir 2>/dev/null)" != .git ]; then
  rm -rf "$ap"
  git clone --filter=blob:none --no-checkout https://github.com/ArduPilot/ardupilot.git "$ap"
fi
# A commit or a tag, such as Copter-4.7.1 for the catalogue (S18).
git -C "$ap" fetch --quiet origin "$ARDUPILOT_COMMIT"
# Forced: a checkout restored without its files (a CI cache) gets them back.
git -C "$ap" checkout --quiet --force FETCH_HEAD
git -C "$ap" submodule update --init --recursive --force --depth 1 --jobs 8 \
  modules/waf modules/mavlink modules/DroneCAN/DSDL modules/DroneCAN/dronecan_dsdlc \
  modules/DroneCAN/libcanard modules/DroneCAN/pydronecan modules/lwip modules/littlefs

# scripts/sitl.sh shares the checkout.
[ "${1:-}" = "--checkout-only" ] && exit 0
mkdir -p "$out"

if [ ! -x "$work/emsdk/emsdk" ]; then
  git clone --depth 1 https://github.com/emscripten-core/emsdk.git "$work/emsdk"
fi
"$work/emsdk/emsdk" install "$EMSCRIPTEN_VERSION"
"$work/emsdk/emsdk" activate "$EMSCRIPTEN_VERSION" >/dev/null
# shellcheck disable=SC1091
source "$work/emsdk/emsdk_env.sh" >/dev/null

cd "$ap"
python3 -m venv "$work/venv" >/dev/null 2>&1 || true
# shellcheck disable=SC1091
source "$work/venv/bin/activate"
pip install --quiet empy==3.3.4 pexpect future lxml
./waf configure --board wasm

vehicles=("$@")
[ ${#vehicles[@]} -gt 0 ] || vehicles=(plane copter rover sub blimp)
for vehicle in "${vehicles[@]}"; do
  program="ardu$vehicle"
  [ "$vehicle" != blimp ] || program=blimp
  started=$(date +%s)
  ./waf build --target "bin/$program"
  cp "build/wasm/bin/$program.js" "$out/"
  cp "build/wasm/bin/$program.wasm" "$out/"
  echo "built $vehicle in $(( $(date +%s) - started )) s"
done
# A downloaded receipt no longer describes files rebuilt locally.
rm -f "$out/download.json"
cat > "$out/SOURCE.txt" <<NOTICE
ArduPilot's software-in-the-loop simulator, built for WebAssembly.
ArduPilot is free software under the GNU General Public License, version 3.
Source: https://github.com/ArduPilot/ardupilot/tree/$(git rev-parse HEAD)
Built with Emscripten $EMSCRIPTEN_VERSION by scripts/simulator.sh.
NOTICE
ls -l "$out"
