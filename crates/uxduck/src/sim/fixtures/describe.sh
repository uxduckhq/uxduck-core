#!/bin/sh
set -eu
if [ -f "$0.delay" ]; then
  sleep "$(cat "$0.delay")"
fi
cat "$0.help"
if [ -f "$0.status" ]; then
  exit "$(cat "$0.status")"
fi
