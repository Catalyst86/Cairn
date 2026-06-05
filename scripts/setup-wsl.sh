#!/usr/bin/env bash
# Cairn — one-shot dev environment setup for WSL2 Ubuntu.
#
# Run AFTER `wsl --install` (Ubuntu) has finished, from inside the Ubuntu shell:
#   cd /mnt/c/Users/danie/Desktop/Cairn && bash scripts/setup-wsl.sh
#
# TIP: building under /mnt/c is slow. For a fast iteration loop, copy the repo
# into the Linux filesystem first:  cp -r /mnt/c/Users/danie/Desktop/Cairn ~/Cairn
set -euo pipefail

echo "==> apt packages (qemu, OVMF firmware, bootloader tooling, build deps)"
sudo apt-get update
sudo apt-get install -y \
  build-essential curl git pkg-config \
  qemu-system-x86 ovmf xorriso mtools \
  just

echo "==> Rust nightly via rustup"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain nightly
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi
rustup toolchain install nightly
rustup component add rust-src llvm-tools rustfmt clippy
rustup target add x86_64-unknown-none

echo "==> Kani (formal verification — honors the verify-from-start decision)"
cargo install --locked kani-verifier
cargo kani setup

cat <<'EOF'

==> Done. Smoke-test sequence:
    cargo test -p cap-core                      # fast host unit tests
    cargo kani -p cap-core --features kani       # prove I2 / I3 / I4 / round-trip
    just run                                     # build keystone + boot in QEMU (serial banner)
EOF
