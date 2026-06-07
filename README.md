# Cairn

**A from-scratch, capability-based microkernel for x86-64 — where one primitive governs memory,
IPC, scheduling, devices, *and* persistent storage, and the kernel gets out of the I/O data path.**

Written in Rust, boots in QEMU via Limine. Built by **James** ([@Catalyst86](https://github.com/Catalyst86))
with **Claude Code** (Anthropic) and **Grok Build** (xAI).

> **Thesis:** *everything is a capability over one persistent substrate, and the kernel leaves the
> data path.* There are no files, file descriptors, UIDs, `open`/`read`/`write`, or `ioctl` — just
> one syscall, `cap_invoke`, and one content-addressed object store.

> ⚠️ **Status:** a research / hobby microkernel. Phases 0–3 are complete and run in QEMU (TCG,
> single-CPU). It is *not* production software and has not yet run on real hardware.

---

## Why Cairn is different

Most of these ideas exist individually (seL4 for verified capabilities, KeyKOS/EROS for
capabilities + persistence, Arrakis/SPDK for kernel-bypass I/O). Cairn's character is the
**synthesis**:

- **One primitive for everything.** Allocating memory, sending a message, scheduling a task,
  driving a disk, and reading a stored object are all `cap_invoke(cap, method, …)` on an
  unforgeable capability. Persistence and I/O live *inside* the security model, not beside it.
- **A machine-checked core, frozen by discipline.** The capability engine (`crates/cap-core`) has
  its invariants proven with [Kani](https://github.com/model-checking/kani)/CBMC — unforgeability,
  O(1) revocation, no rights amplification, type safety — and the frame allocator
  (`crates/frame-alloc`) too. The verified core is treated as immutable; all new state lives in
  side-tables *around* it so the proofs never go stale.
- **The kernel leaves the I/O hot path.** After a one-time `DeviceQueue` capability grant, a ring-3
  driver does a real virtio-blk read — builds the descriptor chain, rings the doorbell, polls the
  used ring — with **zero syscalls**. Trusted domains get the zero-kernel path (`DQ_MAP`); untrusted
  ones get a kernel-mediated, DMA-contained fallback (`DQ_SUBMIT`) — same object, different rights.
- **Storage is content-addressed and survives reboot, as capabilities.** The Cairnlog object store
  is an append-log with an A/B double-buffered superblock (the flip is the single commit point).
  Objects are named by content hash and re-materialize as live `Extent` capabilities on boot.
- **Crash-only by construction.** A ring-3 fault terminates *just that domain*; a supervisor
  re-admits it under a restart budget. The kernel and every other domain live on.

See [DESIGN.md](DESIGN.md) for the ten design pillars and [docs/CAP_ABI.md](docs/CAP_ABI.md) for the
capability format + `cap_invoke` ABI.

## What works today

Boots cleanly in QEMU and demonstrates, per boot, with serial-log proofs:

- **Verified core** — `cap-core` (4 Kani proofs: unforgeability, revocation, no-amplification, type
  safety) and `frame-alloc` (4 Kani proofs: no double-alloc, distinctness, round-trip, bounds).
- **Ring-3 domains** — `syscall`/`sysret`, per-domain capability tables, W^X user pages.
- **EDF scheduler** — earliest-deadline-first with calibrated real-time deadlines; *"time is a
  capability"* (a task is admitted only by presenting a live `TimeSlice` cap).
- **Portal IPC** — synchronous endpoint rendezvous with blocking + zero-copy capability transfer,
  plus async notifications (signal/poll).
- **Crash-only supervision + self-healing** — a faulting domain is terminated and restarted under a
  budget; the kernel survives.
- **Zero-kernel block I/O** — PCI enumeration → polled modern virtio-1.0 driver → a ring-3 driver
  doing a full read with zero syscalls over a `DeviceQueue` cap.
- **Cairnlog object store** — content-addressed, crash-consistent append log; objects survive reboot
  and are re-minted as `Extent` caps; `Extent` MAP brings persisted bytes into a domain.

## Build · run · verify

Developed on Linux (the author uses WSL on a Windows box; any Linux with the toolchain works). The
Rust nightly + components are pinned by [`rust-toolchain.toml`](rust-toolchain.toml).

```bash
# One-time environment (Rust nightly + rust-src, QEMU, Limine/xorriso, Kani, just):
bash scripts/setup-wsl.sh          # or install the equivalents manually

# Tasks (via `just`):
just build-kernel   # cross-compile the keystone microkernel (x86_64-unknown-none)
just run            # build a Limine ISO and boot it in QEMU (serial -> stdout)
just verify         # Kani: prove the capability-core invariants
just test           # fast host unit tests
just check          # rustfmt + clippy
just miri           # Miri UB check on the capability core
```

Re-confirm both verified crates' proofs directly:

```bash
cargo kani -p cap-core    --features kani   # the verified microkernel core (~slow)
cargo kani -p frame-alloc --features kani   # the bitmap frame allocator (~0.2s)
```

The bare-metal `kernel/` is built separately against a bare-metal target (`kernel/.cargo/config.toml`
+ `kernel/linker.ld`); the workspace root holds only the host-verifiable library crates so root-level
`cargo test`/`cargo kani` stay on the host toolchain.

## Repository layout

```
Cairn/
├── kernel/             keystone — the bare-metal microkernel (built separately)
│   ├── src/            scheduler, syscall, paging, capspace, virtio-blk, objstore, …
│   ├── linker.ld       higher-half layout + Limine sections
│   └── limine.conf     bootloader config
├── crates/
│   ├── cap-core/       verified capability table + epoch revocation (no_std, Kani-proved)
│   └── frame-alloc/    verified bitmap frame allocator (no_std, Kani-proved)
├── docs/
│   ├── CAP_ABI.md      capability format, cap_invoke ABI, the proven invariants
│   ├── PHASE3.md       zero-kernel I/O + object store (L0→L4 stack, DMA trust boundary)
│   ├── PORTAL_IPC.md   endpoint rendezvous + capability transfer
│   ├── CRASH_ONLY.md   crash-only domain supervision + restart
│   ├── VERIFICATION.md formal-verification strategy
│   └── KERNEL_BRINGUP.md
├── scripts/            run-qemu.sh, setup-wsl.sh
├── DESIGN.md           the ten design pillars + phased roadmap
└── RESUME.md           detailed engineering log / session handoff
```

## Honest limitations

This is research-grade, and the docs are deliberate about saying so:

- **QEMU-only, single-CPU.** No SMP, no ACPI/IOAPIC/MSI-X yet; the polled I/O path works but
  interrupt-driven I/O is future work.
- **Verification is bounded, not total.** Kani machine-checks the core data structures'
  invariants — not full functional correctness of the whole kernel (that's seL4's league).
- **No IOMMU.** A device queue mapped into a ring-3 driver is write-anywhere DMA; v0 grants it only
  to a *trusted* driver domain, enforced by capability distribution, not the MMU. This is documented
  as an explicit trust boundary with an escalation ladder (validated descriptors → VT-d).

## Roadmap

Phase 0 (foundations) ✅ · Phase 1 (keystone core) ✅ · Phase 2 (domains, EDF, IPC, crash-only) ✅ ·
**Phase 3 (zero-kernel I/O + object store) ✅** · Phase 4 (real hardware: network-boot onto an HPE
ProLiant, the SMP/ACPI retrofit, VT-d) · Phase 5 (confidential boot + a management plane).

## How it's built

A human-directed **Claude Code × Grok Build** collaboration: Grok writes large greenfield Rust,
Claude orchestrates and integrates across files, reviews the `unsafe`/TCB code, drives the
build–boot–verify loop, and runs design + adversarial-review agent panels on the subtle pieces (which
caught real bugs — a data-loss path, a double-free, a DMA-mapping leak).

## License

Dual-licensed under **MIT OR Apache-2.0** (per the workspace manifest), at your option.
