# Cairn OS — Design Document

> Project: **Cairn** (the OS) / **keystone** (the verified microkernel core). Codename, changeable.
>
> **Decisions locked 2026-06-05:** name = Cairn · build now, pace through all phases · **verify from the start** (formal proofs written alongside the code, seL4-style ambition).
> Kicked off 2026-06-05. A collaboration between Claude Code (Anthropic) and Grok Build (xAI), commissioned by James.

## 0. Mandate

Build a brand-new operating system for an HPE ProLiant x86-64 server. It must be **shockingly fast, extremely reliable, very secure, future-proof, and beautiful** — and explicitly **NOT** a Windows/macOS/Linux rehash. We researched what is possible and theoretically possible, then chose the most ambitious design that is actually buildable.

## 1. Hardware target

- HPE ProLiant, x86-64 (Xeon-class, multicore, NUMA), iLO-managed.
- Storage: NVMe/SATA behind an **HPE Smart Array** controller (speaks proprietary PQI/SCSI, *not* standard NVMe/AHCI — see §6 constraint).
- NIC: standard PCIe (e1000 in QEMU; real 1/10GbE on iron).
- Existing iPXE + TFTP + HTTP boot server (we will chainload our kernel over it).
- Currently runs Ubuntu 24.04 (`studio-server`). Cairn will be developed in QEMU first, then network-booted onto the iron.

## 2. The thesis (one sentence)

**Everything is a capability over one persistent substrate, and the kernel gets out of the data path.**

There are no files, no file descriptors, no UIDs, no root, no `ioctl`. There is one primitive — `cap_invoke` — and one continuous, content-addressed, crash-consistent store that erases the file/memory distinction. The trusted core is small enough to (eventually) formally verify, and authorized work touches the kernel **zero** times on the hot path.

## 3. Design pillars (synthesized from independent Claude research + Grok's independent vision)

1. **Capabilities are the only authority** (seL4 + Grok's "nexus"). 128-bit unforgeable capabilities: object id + rights mask (incl. delegate/revoke) + epoch (for O(1) hierarchical revocation) + provenance hash. No ambient authority, no root, no global names. Instantly yank access from a compromised service by bumping an epoch.
2. **Framekernel + exokernel hybrid, tiny verifiable TCB** (Asterinas + MIT exokernel + seL4). All `unsafe`/privileged code confined to a ~10–15K-LoC Rust core (`keystone`) that owns only the MMU/IOMMU, timers, interrupt routing, and the capability table. Drivers, FS, networking are safe-Rust user-space *principals* — replaceable, versioned, restartable without touching the core.
3. **Language-enforced compartments with hardware fallback** (Singularity/Theseus/RedLeaf + Intel MPK/CET). Cooperating trusted domains share one address space and switch in *tens of cycles* via MPK + shadow stacks; mutually distrusting domains get hardware page-table isolation. Defense-in-depth: Rust safety **and** MPK **and** optional page tables.
4. **Zero-kernel data path** (exokernel + io_uring/SPDK + Grok). After a one-time capability grant, a domain maps NVMe/NIC queues directly into its address space and issues I/O with **no kernel crossings**. Completion-based, shared-nothing, thread-per-core.
5. **Orthogonal persistence / single-level store** (Grok + persistent-memory research, adapted). No filesystem. Persistent state = capability-named, content-addressed, copy-on-write, Merkle-checksummed extents in a log-structured store. (Optane is dead → we target **CXL-tiered memory** + NVMe: hot DRAM / cold pooled CXL as a first-class abstraction.)
6. **Time is a capability — EDF scheduling + "resonance bundles"** (Grok + Caladan/Shenango). A domain can't run without a CPU-time capability naming a core set + deadline. Cooperating domains get gang-scheduled adjacent-core quanta with synchronized handoff. Dedicated poll-mode data-plane cores; µs-scale core reallocation; tail-latency engineered.
7. **Confidential-by-default, attested measured boot** (AMD SEV-SNP / Intel TDX). Cairn boots as an attestable enclave with encrypted memory and a hardware root of trust — the single highest-leverage security feature available on x86 today.
8. **Crash-only, live-updatable, spill-free components** (Theseus + Erlang-style supervision). Components hold no hidden cross-state, so a panicked principal is simply terminated and its caps revoked; clients hold their own checkpoint caps. Enables hot-swap upgrades and self-healing — the reliability story.
9. **One ABI primitive** — `cap_invoke(target_cap, arg_regs…, transfer_cap)`. Directories, devices, services are all just capabilities answering methods. Native apps written in Rust/Zig treat caps as **linear types** (can't duplicate a unique right); a restricted WASM dialect hosts less-trusted code; an optional POSIX-compat domain exists for legacy at a deliberate performance cost.
10. **AI-native, safely** (research: substance vs. hype). Learned schedulers/prefetchers are **advisory only, never on the data path**; an offline LLM tunes policies (e.g. eBPF-style scheduler params) between runs. This is also where the Claude×Grok partnership becomes part of Cairn's own operations.

**"Beautiful" for a server OS** = the management/observability plane: a gorgeous real-time control surface showing the live capability graph, latency heatmaps, attestation state, and domain health — not a desktop.

## 4. Why this is novel (not a rehash)

- No files, no FDs, no root, no `ioctl` — a genuinely different system model.
- Monolith-class performance with a microkernel-class trusted surface (framekernel).
- The kernel is absent from the hot path entirely.
- Persistence is orthogonal: your data structures *are* the storage.
- Security is structural (capabilities + tiny TCB + confidential boot), not bolted on.

## 5. Honest scope & reality checks

- **This is a 12+ month effort for a genuinely useful system.** We deliver incrementally; a bootable proof-of-concept comes early (Phase 1).
- **CHERI hardware capabilities are NOT available on x86** (Arm/RISC-V only through ≥2026). We enforce capabilities in software + MPK/CET/IOMMU; we stay ready for x86 memory tagging when it lands.
- **Full formal verification is a multi-year research effort.** We scope verification ambition to the tiny TCB + crypto, aspirationally — not the whole system on day one.
- **HPE Smart Array speaks proprietary PQI, not standard NVMe/AHCI.** Plan: develop in QEMU (virtio/NVMe), boot on iron via iPXE, put the Smart Array in **HBA/pass-through mode** (or net/RAM-boot) initially; a from-scratch PQI driver is a large, optional, later effort.

## 6. Phased roadmap

| Phase | Goal | Key deliverable | Rough effort |
|------|------|-----------------|--------------|
| **0 — Foundations** | Lock spec, scaffold, dev loop | Rust workspace + `cap_invoke` ABI spec + Limine "hello world" booting in QEMU printing to serial; CI | days |
| **1 — keystone core** | The verifiable kernel | GDT/IDT/interrupts, frame allocator, paging, heap, **capability table + epoch revocation**, `cap_invoke`; boots in QEMU with tests | weeks |
| **2 — Domains & scheduling** | Isolation + time | Ring-3/MPK domains, portal IPC (shared-mem + notify caps), **EDF scheduler w/ time-caps**, crash-only supervision; kill+restart a domain | weeks |
| **3 — Zero-kernel I/O** | Data path + persistence | PCIe enum, virtio-blk/net, direct queue mapping, **log-structured object store + extent caps**; objects survive reboot | weeks |
| **4 — Real iron** | Boot on the HPE server | Chainload over existing iPXE/HTTP; serial+framebuffer on iron; SMP/ACPI (MADT/SRAT), x2APIC, NUMA; Smart Array HBA mode; real NIC | weeks |
| **5 — Confidential + beautiful** | Harden + management plane | SEV-SNP/TDX attested boot; the live capability-graph + latency control surface; scope TCB verification; offline-LLM policy tuning | ongoing |

## 7. Division of labor — Claude × Grok

Based on Grok's own honest self-assessment.

**Grok (grok-build / grok-composer-2.5-fast):** strongest at greenfield Rust systems code, holds an ~8–12K-LoC subsystem coherently, fast correct-by-construction modules with tests, native reasoning in capabilities + x86 hardware (MSRs, VT-d, MPK, NVMe queues). **Owns:** keystone core primitives, capability table + epoch revocation, the extent/object store + log, the EDF scheduler + resonance bundles, the `cap_invoke` ABI codegen/runtime, QEMU integration-test harnesses, restartable domain supervisors.

**Claude (me):** orchestration + cross-file integration, the **actual hardware loop** (I can drive the iPXE boot server, iLO, QEMU, and run/observe builds), driver bring-up & debugging on real iron, the **beautiful management-plane frontend**, security review of Grok's `unsafe` TCB code, formal-spec scoping, and keeping invariants across the whole tree. (Grok explicitly flagged it can't reboot/observe the iron or do heavy frontend — my lane.)

**Both, adversarially:** I review Grok's unsafe core for soundness; Grok writes property-based tests for revocation/sharing; we cross-check the capability model against the threat model.

## 8. Open decisions for James

1. **Name** — keep "Cairn/keystone" or pick another (Lattice, Aether, Cairn, …)?
2. **How far this session** — just lock the plan, or start **Phase 0** now (scaffold the repo, have Grok generate the first capability-core crate, and get a Limine kernel booting in QEMU)?
3. **Verification ambition** — aspirational (design for verifiability, prove later) vs. aggressive (formal proofs from the start, slower)?

## Sources (selected)
seL4 performance/verification · Asterinas framekernel · Theseus (OSDI'20) · MIT exokernel (SOSP'97) · Singularity/Midori (MSR) · RedLeaf (OSDI'20) · Caladan/Shenango/Skyloft · DPDK/SPDK · AMD SEV-SNP / Intel TDX · CXL 2.0/3.0 · rust-osdev / Limine / Writing an OS in Rust · HPE smartpqi advisory.
