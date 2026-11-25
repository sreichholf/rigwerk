#!/usr/bin/env bash
# Builds the Steinberg VST3 SDK sample plugins the `vst3_fixture` tests load
# (again, adelay, host-checker) and copies their bundles to <out-dir>/plugins.
# Point RIGWERK_VST3_FIXTURES at that folder to run the tests.  The bundles
# are build products and are never committed.  Runs on Linux and in Git Bash
# on Windows (Visual Studio generator there, Ninja on Linux).
#
# The tag is pinned to the commit it pointed at when it was chosen.  A tag
# that has been moved fails the check instead of quietly changing the build.
set -euo pipefail

SDK_TAG=v3.8.1_build_84
# A lightweight tag: this is the tag ref itself.
SDK_COMMIT=3cdf9ca5d1f5b1b21e0a86832aa4abe55607bd96
SDK_URL=https://github.com/steinbergmedia/vst3sdk
TARGETS=(again adelay host-checker)

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <out-dir>" >&2
  exit 2
fi

mkdir -p "$1"
# Relative paths from here on, so native Windows CMake never sees an MSYS path.
cd "$1"

# The bundles count as built only when every one is there and the tag file
# names this tag and commit.  Each bundle is moved into place after a full
# copy, so an interrupted run never leaves a half bundle that looks finished.
have_all() {
  local target
  [[ "$(cat plugins/.sdk-tag 2>/dev/null)" == "$SDK_TAG $SDK_COMMIT" ]] || return 1
  for target in "${TARGETS[@]}"; do
    [[ -d "plugins/$target.vst3" ]] || return 1
  done
}

if have_all; then
  echo "VST3 fixtures already in $(pwd)/plugins"
  exit 0
fi

if [[ -d vst3sdk/.git ]] &&
  [[ "$(git -C vst3sdk describe --tags --exact-match 2>/dev/null)" != "$SDK_TAG" ]]; then
  rm -rf vst3sdk build
fi
if [[ ! -d vst3sdk/.git ]]; then
  rm -rf vst3sdk
  # core.longpaths: the VSTGUI tree is deeper than MAX_PATH allows on Windows.
  git -c core.longpaths=true clone --depth 1 --branch "$SDK_TAG" "$SDK_URL" vst3sdk
fi
if [[ "$(git -C vst3sdk rev-parse HEAD)" != "$SDK_COMMIT" ]]; then
  echo "error: $SDK_TAG is not $SDK_COMMIT any more" >&2
  exit 1
fi
git -C vst3sdk -c core.longpaths=true checkout --quiet --force "$SDK_COMMIT"
# Idempotent, and it finishes a clone that was interrupted here.
git -C vst3sdk -c core.longpaths=true submodule update --init --depth 1 \
  base cmake pluginterfaces public.sdk vstgui4

options=(
  -DSMTG_ENABLE_VST3_PLUGIN_EXAMPLES=ON
  -DSMTG_ENABLE_VST3_HOSTING_EXAMPLES=OFF
  -DSMTG_ENABLE_VSTGUI_SUPPORT=ON
  -DSMTG_RUN_VST_VALIDATOR=OFF
  -DSMTG_CREATE_PLUGIN_LINK=OFF
)

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    cmake -S vst3sdk -B build "${options[@]}"
    cmake --build build --config Release --parallel --target "${TARGETS[@]}"
    ;;
  *)
    cmake -S vst3sdk -B build -G Ninja -DCMAKE_BUILD_TYPE=Release "${options[@]}"
    ninja -C build "${TARGETS[@]}"
    ;;
esac

mkdir -p plugins
rm -f plugins/.sdk-tag
for target in "${TARGETS[@]}"; do
  rm -rf "plugins/$target.vst3" "plugins/$target.vst3.partial"
  cp -RL "build/VST3/Release/$target.vst3" "plugins/$target.vst3.partial"
  mv "plugins/$target.vst3.partial" "plugins/$target.vst3"
done
echo "$SDK_TAG $SDK_COMMIT" > plugins/.sdk-tag
echo "VST3 fixtures in $(pwd)/plugins"
