#!/usr/bin/env bash
# Headless PipeWire so pw-metadata can read and set clock.force-quantum.
# There is no sound card here; this is the settings server, not playback.
set -euo pipefail

runtime="${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}"
mkdir -p "${runtime}"
chmod 700 "${runtime}"
export XDG_RUNTIME_DIR="${runtime}"

if [[ -S "${runtime}/pipewire-0" ]] && pw-metadata -n settings >/dev/null 2>&1; then
  exit 0
fi

rm -f "${runtime}/pipewire-0"
exec pipewire
