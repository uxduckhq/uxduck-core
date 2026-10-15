# Source after setting root: only reusable output lives here; compiler work
# stays under target, where it can still be removed to recover disk space.
sim_cache="${UXDUCK_SIM_CACHE_DIR:-$root/.cache/simulators}"
case "$sim_cache" in
  /*) ;;
  *) sim_cache="$root/$sim_cache" ;;
esac
