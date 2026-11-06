#!/usr/bin/env bash
# System packages, a Rust toolchain new enough for eframe 0.37 (MSRV 1.95),
# and a warm crates.io cache. Must exit. PipeWire itself is started by start.sh.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  pkg-config \
  libasound2-dev \
  libclang-dev \
  libegl1-mesa-dev \
  libwayland-dev \
  libxcursor-dev \
  libxi-dev \
  libxkbcommon-dev \
  libxrandr-dev \
  pipewire \
  pipewire-alsa \
  pipewire-bin \
  pipewire-pulse \
  libpipewire-0.3-dev \
  wireplumber

# Login shells (install, start, and the agent) source /etc/profile.d.
# Keep the default unset so a platform-provided runtime dir still wins.
sudo tee /etc/profile.d/rigwerk-pipewire.sh >/dev/null <<'EOF'
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}"
EOF

if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal \
      --component rustfmt,clippy,rust-analyzer
  for bin in rustup cargo rustc rustdoc rustfmt cargo-fmt cargo-clippy clippy-driver rust-analyzer; do
    if [[ -x "${HOME}/.cargo/bin/${bin}" ]]; then
      sudo ln -sfn "${HOME}/.cargo/bin/${bin}" "/usr/local/bin/${bin}"
    fi
  done
fi

rustup toolchain install stable --profile minimal --component rustfmt,clippy,rust-analyzer
rustup default stable

# Workspace root is /agent when the environment sets workDir. The checkout
# lives under /agent/repos, and a draft build may start elsewhere.
if [[ -d /agent/repos/rigwerk ]]; then
  cd /agent/repos/rigwerk
fi
if [[ -f Cargo.toml ]]; then
  cargo fetch --locked
fi
