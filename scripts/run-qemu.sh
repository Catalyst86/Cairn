#!/usr/bin/env bash
#
# scripts/run-qemu.sh
# Boot the keystone kernel under QEMU using a Limine bootable image.
#
# What it does:
#   1. Downloads a Limine binary release tarball (if the extracted tree is absent)
#      and builds the BIOS+UEFI bits (requires make, gcc, etc on first run).
#   2. Assembles a bootable directory tree (UEFI FAT layout) containing:
#        - the Limine UEFI loader (BOOTX64.EFI)
#        - limine.conf
#        - the keystone ELF at /boot/keystone
#   3. Launches qemu-system-x86_64 with:
#        - q35 machine
#        - 512 MiB RAM
#        - serial redirected to stdio (so the banner appears on your terminal)
#        - an OVMF UEFI firmware image (so we boot via UEFI)
#        - a virtio-blk drive backed by the FAT directory (rw for easy iteration)
#
# Usage (after you have a Rust toolchain + cargo):
#   cargo build -p keystone --target kernel/x86_64-cairn-none.json
#   ./scripts/run-qemu.sh
#
#   For a release build:
#   cargo build -p keystone --target kernel/x86_64-cairn-none.json --release
#   ./scripts/run-qemu.sh
#
# You can pass extra qemu args after -- :
#   ./scripts/run-qemu.sh -- -s -S          # for gdb stub
#
# Paths you will almost certainly need to adjust for your machine are marked
# with "ADJUST:" below.
#
set -euo pipefail

# ---------- configuration (ADJUST these for your host) -------------------------

# Limine version to fetch. Pick any reasonably recent v7 or v8 release.
# If the URL 404s, just bump the version or switch to a tag you have locally.
LIMINE_VERSION="${LIMINE_VERSION:-8.0.12}"
LIMINE_TARBALL="limine-${LIMINE_VERSION}.tar.gz"
LIMINE_DIR="limine-${LIMINE_VERSION}"

# Where to find an OVMF (UEFI) firmware image.
# Common locations:
#   Arch/Manjaro:   /usr/share/edk2/x64/OVMF.fd
#   Debian/Ubuntu:  /usr/share/OVMF/OVMF.fd  or /usr/share/qemu/OVMF.fd
#   Fedora:         /usr/share/edk2/ovmf/OVMF.fd
#   macOS (brew):   $(brew --prefix)/share/qemu/OVMF.fd  (or edk2)
#   WSL2/Windows:   often /usr/share/edk2/x64/OVMF.fd after apt install ovmf
#   You can also point at a local copy you downloaded from https://github.com/retrage/edk2-nightly
OVMF_PATH="${OVMF_PATH:-/usr/share/edk2/x64/OVMF.fd}"

# QEMU binary (override with QEMU=... if you have qemu-system-x86_64 elsewhere)
QEMU="${QEMU:-qemu-system-x86_64}"

# Memory given to the VM (the task asks for 512M)
MEM="512M"

# Directory we will turn into a FAT filesystem for the UEFI drive.
IMAGE_DIR="qemu_fat"

# ---------- 1. obtain Limine ----------------------------------------------------

if [ ! -d "$LIMINE_DIR" ]; then
    echo "==> Downloading Limine v${LIMINE_VERSION} (binary/source release)..."
    curl -fL -o "$LIMINE_TARBALL" \
        "https://github.com/limine-bootloader/limine/releases/download/v${LIMINE_VERSION}/limine-${LIMINE_VERSION}.tar.gz"

    echo "==> Extracting..."
    tar -xzf "$LIMINE_TARBALL"
    rm -f "$LIMINE_TARBALL"

    echo "==> Building Limine (needs autotools/make/gcc on first use)..."
    pushd "$LIMINE_DIR" >/dev/null
    # Some releases ship ./bootstrap, some don't in the tarball.
    if [ -x ./bootstrap ]; then
        ./bootstrap || true
    fi
    ./configure --enable-bios --enable-uefi --enable-uefi-cd >/dev/null
    make -j"$(nproc 2>/dev/null || echo 4)" >/dev/null
    popd >/dev/null
    echo "==> Limine built."
fi

# After a successful build the UEFI loader lives at:
LIMINE_EFI="$LIMINE_DIR/BOOTX64.EFI"
if [ ! -f "$LIMINE_EFI" ]; then
    # Some build layouts put it under bin/ or uefi/
    LIMINE_EFI="$(find "$LIMINE_DIR" -name 'BOOTX64.EFI' | head -n1 || true)"
fi
if [ -z "${LIMINE_EFI:-}" ] || [ ! -f "$LIMINE_EFI" ]; then
    echo "ERROR: Could not locate BOOTX64.EFI after building Limine in $LIMINE_DIR"
    echo "       (the configure/make step may have failed silently)."
    exit 1
fi

# ---------- 2. build the bootable FAT image tree -------------------------------

echo "==> Assembling boot image in $IMAGE_DIR/"

# Clean previous
rm -rf "$IMAGE_DIR"
mkdir -p "$IMAGE_DIR/EFI/BOOT" "$IMAGE_DIR/boot"

# Copy the UEFI bootloader that Limine produces.
cp "$LIMINE_EFI" "$IMAGE_DIR/EFI/BOOT/BOOTX64.EFI"

# Copy our hand-written Limine config (must be at the root of the volume).
cp kernel/limine.conf "$IMAGE_DIR/limine.conf"

# Locate the just-built keystone ELF.
# We prefer release, then debug. The user is responsible for having run cargo build.
KERNEL_CANDIDATES=(
    "target/x86_64-cairn-none/release/keystone"
    "target/x86_64-cairn-none/debug/keystone"
)
KERNEL_ELF=""
for c in "${KERNEL_CANDIDATES[@]}"; do
    if [ -f "$c" ]; then
        KERNEL_ELF="$c"
        break
    fi
done

if [ -z "$KERNEL_ELF" ]; then
    echo "ERROR: Keystone ELF not found."
    echo "       Build it first, e.g.:"
    echo "         cargo build -p keystone --target kernel/x86_64-cairn-none.json"
    echo "       or with --release."
    exit 1
fi

echo "    Using kernel: $KERNEL_ELF"
cp "$KERNEL_ELF" "$IMAGE_DIR/boot/keystone"

# ---------- 3. locate OVMF if the default path is wrong ------------------------

if [ ! -f "$OVMF_PATH" ]; then
    echo "==> OVMF not found at $OVMF_PATH, searching common paths..."
    for p in \
        /usr/share/edk2/x64/OVMF.fd \
        /usr/share/OVMF/OVMF.fd \
        /usr/share/qemu/OVMF.fd \
        /usr/share/edk2/ovmf/OVMF.fd \
        /opt/homebrew/share/qemu/OVMF.fd \
        /usr/local/share/qemu/OVMF.fd
    do
        if [ -f "$p" ]; then
            OVMF_PATH="$p"
            echo "    Found: $OVMF_PATH"
            break
        fi
    done
fi

if [ ! -f "$OVMF_PATH" ]; then
    echo "ERROR: Could not find an OVMF.fd UEFI firmware image."
    echo "       Install the ovmf/edk2 package for your distro or set OVMF_PATH=/path/to/OVMF.fd"
    echo "       You can download nightly builds from https://github.com/retrage/edk2-nightly"
    exit 1
fi

# ---------- 4. launch QEMU ------------------------------------------------------

if ! command -v "$QEMU" >/dev/null 2>&1; then
    echo "ERROR: $QEMU not found in PATH. Install qemu-system-x86_64."
    exit 1
fi

echo "==> Launching QEMU (serial on stdout, OVMF at $OVMF_PATH)"
echo "    You should see the exact banner:"
echo "      Cairn keystone v0 — the core is alive"
echo "    followed by the memory map line."
echo

"$QEMU" \
    -M q35 \
    -m "$MEM" \
    -serial stdio \
    -bios "$OVMF_PATH" \
    -drive file=fat:rw:"$IMAGE_DIR",format=raw,if=virtio \
    -no-reboot \
    -display none \
    -name "Cairn-keystone-v0" \
    "$@"

# When you Ctrl-C or the VM shuts down you return here.
echo
echo "QEMU exited."
