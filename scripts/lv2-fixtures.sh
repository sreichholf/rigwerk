#!/usr/bin/env bash
# Builds the LV2 example plugins the `lv2_fixture` tests load (eg-amp,
# eg-params, eg-sampler) and copies their bundles to <out-dir>/plugins.
# Point RIGWERK_LV2_FIXTURES at that folder to run the tests.  The bundles
# are build products and are never committed.  Linux and macOS, POSIX shell
# tools; Windows CI does not build these yet.
#
# eg-amp covers load, process and the panel; eg-params covers state (it has
# only atom ports, no audio); eg-sampler covers state:loadDefaultState and
# the worker.  eg-sampler needs libsndfile and libsamplerate to build.
#
# The tag is pinned to the commit it pointed at when it was chosen.  A tag
# that has been moved fails the check instead of quietly changing the build.
set -euo pipefail

LV2_TAG=v1.18.10
# An annotated tag: this is the commit refs/tags/v1.18.10 peels to.
LV2_COMMIT=0bcde338db1c63bbc503b4d1f6d7b55ed43154af
LV2_URL=https://github.com/lv2/lv2
BUNDLES=(eg-amp eg-params eg-sampler)

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <out-dir>" >&2
  exit 2
fi

for tool in meson ninja; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool is required; install meson and ninja" >&2
    exit 1
  fi
done

mkdir -p "$1"
cd "$1"

# The bundles count as built only when every one is there and the tag file
# names this tag and commit.  Each bundle is moved into place after a full
# copy, so an interrupted run never leaves a half bundle that looks finished.
have_all() {
  local bundle
  [[ "$(cat plugins/.lv2-tag 2>/dev/null)" == "$LV2_TAG $LV2_COMMIT" ]] || return 1
  for bundle in "${BUNDLES[@]}"; do
    [[ -d "plugins/$bundle.lv2" ]] || return 1
  done
}

if have_all; then
  echo "LV2 fixtures already in $(pwd)/plugins"
  exit 0
fi

if [[ -d lv2/.git ]] &&
  [[ "$(git -C lv2 describe --tags --exact-match 2>/dev/null)" != "$LV2_TAG" ]]; then
  rm -rf lv2 build stage
fi
if [[ ! -d lv2/.git ]]; then
  rm -rf lv2
  git clone --depth 1 --branch "$LV2_TAG" "$LV2_URL" lv2
fi
if [[ "$(git -C lv2 rev-parse HEAD)" != "$LV2_COMMIT" ]]; then
  echo "error: $LV2_TAG is not $LV2_COMMIT any more" >&2
  exit 1
fi
git -C lv2 checkout --quiet --force "$LV2_COMMIT"

options=(
  --buildtype=release
  -Ddocs=disabled
  -Dtests=disabled
  -Dplugins=auto
  -Dprefix=/
  -Dlv2dir=bundles
)
if [[ -d build ]]; then
  meson setup --reconfigure build lv2 "${options[@]}"
else
  meson setup build lv2 "${options[@]}"
fi
ninja -C build

rm -rf stage
DESTDIR="$PWD/stage" meson install -C build

mkdir -p plugins
rm -f plugins/.lv2-tag
for bundle in "${BUNDLES[@]}"; do
  if [[ ! -d "stage/bundles/$bundle.lv2" ]]; then
    echo "error: $bundle was not built; install libsndfile and libsamplerate" >&2
    exit 1
  fi
done
for bundle in "${BUNDLES[@]}"; do
  rm -rf "plugins/$bundle.lv2" "plugins/$bundle.lv2.partial"
  cp -R "stage/bundles/$bundle.lv2" "plugins/$bundle.lv2.partial"
  mv "plugins/$bundle.lv2.partial" "plugins/$bundle.lv2"
done
echo "$LV2_TAG $LV2_COMMIT" > plugins/.lv2-tag
echo "LV2 fixtures in $(pwd)/plugins"
