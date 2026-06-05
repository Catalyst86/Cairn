# Cairn task runner. Run inside the WSL2 Ubuntu dev environment.
# `just` is apt-installable (`sudo apt install just`).

# List recipes
default:
    @just --list

# Cross-compile the keystone microkernel (bare-metal target)
build-kernel:
    cd kernel && cargo build --release

# Build a bootable Limine image and run it in QEMU (serial to stdout)
run: build-kernel
    bash scripts/run-qemu.sh

# Formally verify the capability core with Kani
verify:
    cargo kani -p cap-core

# Fast host unit tests for the verifiable crates
test:
    cargo test -p cap-core

# Format and lint
check:
    cargo fmt --all
    cargo clippy -p cap-core -- -D warnings

# Detect undefined behavior in the capability core under Miri
miri:
    cargo +nightly miri test -p cap-core
