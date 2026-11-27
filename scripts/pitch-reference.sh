#!/usr/bin/env bash
# Renders the pitch harness's inputs through Rubber Band R3 (offline) as the
# "best achievable" reference the phase 0 scores are calibrated on.  Run the
# harness once with RIGWERK_PITCH_RENDER=<dir> first, so <dir> holds the
# <case>_input.wav files; this writes <dir>/reference/<case>_<shift>.wav,
# and the next harness run scores them.  Needs the rubberband CLI (3.x,
# for -3); it is never a build dependency and nothing of it is linked.
set -euo pipefail

SHIFTS=(-1 -2 -4 -5 -7 -12 +5 +12)

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <render-dir>" >&2
  exit 2
fi
if ! command -v rubberband >/dev/null 2>&1; then
  echo "error: the rubberband CLI is required (e.g. apt-get install rubberband-cli)" >&2
  exit 1
fi

dir=$1
shopt -s nullglob
inputs=("$dir"/*_input.wav)
if [[ ${#inputs[@]} -eq 0 ]]; then
  echo "error: no *_input.wav in $dir; run the harness with RIGWERK_PITCH_RENDER=$dir first" >&2
  exit 1
fi
mkdir -p "$dir/reference"
for input in "${inputs[@]}"; do
  name=$(basename "$input" _input.wav)
  for shift in "${SHIFTS[@]}"; do
    # R3 (the finer engine), pitch only, no formant preservation.
    rubberband -3 -q -p "$shift" "$input" "$dir/reference/${name}_${shift}.wav"
  done
done
echo "wrote $dir/reference"
