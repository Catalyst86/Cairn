# Cairn

A from-scratch, capability-based operating system for x86-64 servers.
Built by **James** with **Claude Code** (Anthropic) and **Grok Build** (xAI).

> **Thesis:** everything is a capability over one persistent substrate, and the
> kernel gets out of the data path. No files, no file descriptors, no root, no
> `ioctl` — one primitive, `cap_invoke`, and one content-addressed store.

See [`DESIGN.md`](DESIGN.md) for the full architecture, [`docs/CAP_ABI.md`](docs/CAP_ABI.md)
for the capability ABI, and [`docs/VERIFICATION.md`](docs/VERIFICATION.md) for the
verify-from-the-start strategy.

## Layout

```
Cairn/
├── DESIGN.md              Architecture & roadmap
├── docs/
│   ├── CAP_ABI.md         The cap_invoke ABI + capability format + invariants
│   └── VERIFICATION.md    Formal-verification strategy (Kani / Creusot / Miri)
├── kernel/                keystone — the bare-metal microkernel core (separate build)
├── crates/
│   └── cap-core/          Verified capability table + revocation (no_std, Kani-proved)
├── scripts/               run-qemu.sh, image build, etc.
└── Cargo.toml             Workspace (host-verifiable crates only)
```

## Dev environment

Cairn is developed in **WSL2 Ubuntu** on the Windows workstation (Linux-first
verification + bootloader tooling), targeting QEMU first, then the real HPE
ProLiant via network boot.

One-time setup (inside WSL2 Ubuntu):

```bash
# Rust nightly with bare-metal support (rust-toolchain.toml pins the rest)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add rust-src llvm-tools
# Bootloader + emulator + image tooling
sudo apt update && sudo apt install -y qemu-system-x86 ovmf xorriso mtools build-essential just
# Formal verification
cargo install --locked kani-verifier && cargo kani setup
```

## Build / run / verify

```bash
just build-kernel   # cross-compile the keystone microkernel
just run            # build a Limine image and boot it in QEMU (serial -> stdout)
just verify         # Kani: prove the capability-core invariants
just test           # fast host unit tests
just check          # fmt + clippy
```

## Status

Phase 0 (foundations). Tracked in the session task board; roadmap in `DESIGN.md`.
