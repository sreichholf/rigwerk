#!/usr/bin/env bash
# Runs the upstream nam-rs checks against vendor/nam-rs: rustfmt, clippy,
# its unit tests, parity against the NAM Core and Python trainer fixtures,
# and real-time safety.
# Clones nam-rs at the tag the vendored copy is based on into <dir>/nam-rs,
# puts vendor/nam-rs/src in place of its src/ and runs `cargo test` there.
# The upstream fixtures are several MB and are never committed here.
set -euo pipefail

NAM_RS_TAG=v0.4.0
NAM_RS_COMMIT=97fba1ea22dc19e68d408adf2abd72252af5e4d6
NAM_RS_URL=https://github.com/OpenSauce/nam-rs

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <dir>" >&2
  exit 2
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$1"
dir=$(cd "$1" && pwd)
checkout="$dir/nam-rs"

if [[ ! -d "$checkout/.git" ]]; then
  git -c advice.detachedHead=false clone --quiet --depth 1 \
    --branch "$NAM_RS_TAG" "$NAM_RS_URL" "$checkout"
fi
if [[ "$(git -C "$checkout" rev-parse HEAD)" != "$NAM_RS_COMMIT" ]]; then
  echo "error: $NAM_RS_TAG is not $NAM_RS_COMMIT any more" >&2
  exit 1
fi
git -C "$checkout" checkout --quiet --force "$NAM_RS_COMMIT"

rm -rf "$checkout/src"
cp -R "$repo/vendor/nam-rs/src" "$checkout/src"

cd "$checkout"
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
# The allocation check: rt_safety installs its failing allocator only with
# debug assertions, so a release run of it would prove nothing.
cargo test --locked --test rt_safety
# Everything, optimised: the numbers the tiles produce once they vectorize.
cargo test --locked --release
