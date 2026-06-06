# Cairn — Next-Session Handoff Prompt

> Paste everything below the line into a fresh Claude Code session started in
> `C:\Users\danie\Desktop\Cairn`.

---

You are continuing **Cairn**, a from-scratch, capability-based operating system (verifiable
microkernel core called **keystone**) for James's HPE ProLiant x86-64 server, written in Rust
and built as a **Claude Code × Grok Build** collaboration. Repo (canonical — EDIT HERE):
`C:\Users\danie\Desktop\Cairn`. It boots in QEMU (Limine, BIOS+UEFI) and is developed in WSL1.

## STEP 1 — Orient before doing anything (read in this order, don't skip)
1. `RESUME.md` — the canonical session handoff: current status, the exact next steps, dev
   environment, build/verify commands, gotchas, and the full roadmap. **This is the source of truth.**
2. `docs/PHASE3.md` — the active phase's architecture (the L0→L4 I/O stack), the increment
   roadmap (INC1–INC7), the honest no-IOMMU DMA trust boundary, and the crash-consistency design.
3. `docs/DESIGN.md` (the 10 design pillars + phased roadmap) and `docs/CAP_ABI.md` (the
   `cap_invoke` capability ABI). The `cairn-os` memory is auto-loaded for high-level context.
4. Run `git log --oneline -20` to see the commit chain, then skim the most recent commit bodies.

## STEP 2 — Confirm it still boots clean (do this first, every session)
Run via the **PowerShell tool** (NOT the Bash tool — git-bash mangles `/mnt/c` paths):
`wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-go-kernel.sh`
It rsyncs `kernel/`, builds, makes a Limine ISO, runs QEMU ~20s (serial → `/root/cairn-serial.log`
+ stdout). Filter in PowerShell with `... | Select-String -Pattern "..."` (no `grep`). Expect (no
panic/fault anywhere):
- Phase 3 (early, after the heap): a `pci …` device list including `pci 00:03.0 vendor=0x1af4
  device=0x1042 … virtio-blk`; `virtio-blk: ready (queue0 size=128)`; `virtio-blk: wrote+read
  LBA32760 512B match=true (flush negotiated=true)`; `objstore: mounted superblock seq=N …` (or
  `objstore: formatted -> seq=1` on a fresh disk); the INC5 extent proof `extent: put lba=L len=59
  hash=0x7b4ded… ; X_READ=>Ok reply_hash=0x7b4ded… ; … content-addressed match=true` then `extent:
  READ-masked cap X_READ=>ErrRights … X_WRITE=>ErrMethod`; and the INC6 recovery proof `objstore:
  recovered root Extent cptr=0 lba=L … objects-survive-reboot=true` (or `no committed root to recover
  (fresh store)` on the first boot after `rm`). The store persists — each boot `seq` grows, the same
  bytes re-`put` to a fresh `lba` with the SAME hash (CoW), and `recover()` re-mints the prior boot's
  committed root from disk.
- Phase 2 (after the cap self-tests, post-`sti`): the crash-only self-healing loop
  (`domain 4 … terminated: #UD` → `supervisor: RESTARTED …` ×2 → `reaped`), then the endpoint
  rendezvous (`ep: domain2 E_RECV resumed … recv_cptr=3`, MOVE proof `cptr=1 => status=1`), plus
  `perdomain:`/`notify:`/`GRANT_CAP gate`.

## Where we are
- **Phases 0, 1, 2 are COMPLETE.** Phase 2 = ring-3 domains, EDF + time-capabilities, portal IPC
  (blocking endpoint rendezvous + capability transfer + scheduler block/wake), and crash-only
  domain supervision **+ restart/self-healing**.
- **Phase 3 (zero-kernel I/O + object store) is UNDERWAY:** INC1 PCI enum ✅, INC2 virtio-blk read
  ✅, INC3 write ✅, INC4 Cairnlog superblock + content hash + `flush` ✅, INC5 append-log `put` +
  content-addressed Extent caps ✅ (adversarial panel caught + fixed a mount() reformat-on-read-error
  data-loss bug and a smoke-test/log LBA collision), INC6 objects-survive-reboot ✅ (T2 milestone:
  `objstore::recover()` re-mints the prior boot's committed root as a live Extent cap from disk, hash
  re-verified — proven 2-run). Last feature commit: `8e66c0f`.

## STEP 3 — Your task: Phase 3 INC7 — zero-kernel DeviceQueue grant + Extent MAP (T1, the namesake)
INC6 (commit `8e66c0f`) closed T2 (objects survive reboot). INC7 is **T1**: the kernel leaves the I/O
hot path after a one-time capability grant (DESIGN.md pillar 4) — **the first live use of
`Rights::MAP`**. This is SUBTLE/DMA-adjacent: do a judged design panel + adversarial review (Workflow
tool) before committing, and re-read the **no-IOMMU DMA trust boundary** in `docs/PHASE3.md` (a mapped
DeviceQueue is write-anywhere DMA → grant ONLY to a TRUSTED driver domain in v0). cap-core stays
byte-unchanged (`DeviceQueue=9`, `Rights::MAP` already exist — only USE them).
- **Paging helpers** (`kernel/src/paging.rs`): `map_user_mmio_page` (a USER + NO_CACHE doorbell page)
  and a **map-existing-phys-frame-at-user-VA** helper (the current `map_user_page` allocates a FRESH
  frame, so it can't map the already-allocated ring frames — you need to map existing phys).
- **DeviceQueue object** (`kernel/src/capspace.rs`, mirror `EXTENTS`/`ENDPOINTS` EXACTLY — const-static
  side-table, never touch cap-core): `DQ_INFO=1`/`DQ_MAP=2`/`DQ_SUBMIT=3` ids; `DEVQUEUES:[DqMeta;
  OBJECT_TABLE_SIZE]`; `create_device_queue`; wire `dispatch_method` `extra`-rights `(DeviceQueue,
  DQ_MAP)=>MAP`, `(DeviceQueue,DQ_SUBMIT)=>WRITE`, `(DeviceQueue,DQ_INFO)=>READ`. `DQ_MAP` maps the
  virtqueue rings + the doorbell page contiguously into a trusted driver domain (bump `MAX_DOMAINS`
  5→6) and returns one base VA.
- **Extent MAP**: extend the Extent path so `MAP` maps the named data sectors (from
  `extent_metadata` — the seed) into a domain's address space (bytes reach a domain via MAP, never a
  register, per CAP_ABI §5).
- **Proof (boot-log):** a ring-3 driver blob, having received a `DeviceQueue` cap, fills a descriptor +
  rings the doorbell + polls `used` with **ZERO syscalls** (true zero-kernel I/O); negative test: a
  MAP-masked copy of the cap ⇒ `ErrRights`. Optional INC8 (may slip to Phase 4): `DQ_SUBMIT`
  kernel-validated descriptors; IRQ completion (`IrqHandler` + `Notification`); VT-d scaffold.

Full plan + the no-IOMMU DMA trust boundary + the escalation ladder (v0 trust → v1 validated descs →
v2 VT-d): `docs/PHASE3.md` INC7 + "DMA trust boundary".

## Hard rules — do not violate
- **cap-core (`crates/cap-core`) is FROZEN and Kani-verified — NEVER edit it.**
  `git diff HEAD -- crates/` must stay EMPTY (the standing regression gate). All new object state
  lives kernel-side in const-static side-tables. `ObjectKind::Extent=8`/`DeviceQueue=9` and all
  needed `Rights` already exist; you only USE them.
- **Dev env is WSL1, not WSL2.** Always invoke WSL through the **PowerShell tool**. PowerShell 5.1
  mangles embedded double-quotes; `<` is a reserved operator (avoid `wc -l < file` in PowerShell).
- **Git commits:** the Bash tool is git-bash. Use `git commit -F <msgfile>` or a bash here-doc
  (`git commit -F - <<'EOF' … EOF`) — do NOT use PowerShell `@'…'@` here-strings (they leak a stray
  `@` into the message). End every commit message with:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
  Commit one increment at a time, only after QEMU verification; then update `RESUME.md` +
  `docs/PHASE3.md`.
- **Kani:** `kani-proofs.sh` HANGS on the frame-alloc proofs (leaves stale `cbmc`). To re-confirm
  cap-core run ONLY `cargo kani -p cap-core --features kani`; reap stragglers with `pkill -9 cbmc`
  (by NAME — `-f` self-matches the kill command and kills your shell).
- **DMA discipline (virtio):** every device-facing address (queue regs, descriptor `addr`) is RAW
  guest-physical (`frame * 4096`); the kernel touches rings/buffers via the HHDM alias
  (`phys + memory::hhdm_offset()`). Mixing them = silent no-completion. Keep the bounded anti-hang
  poll guard.
- **Persistent disk:** `/root/cairn-disk.img` (16 MiB raw, attached as virtio-blk in
  `C:\WSL\cairn-go-kernel.sh`). `rm /root/cairn-disk.img` to reset the store (recreated on next run).
  LBA 0/1 hold the superblock; the log starts at LBA 2.
- **Single-CPU / IRQs-off discipline:** all scheduler + cap-table + side-table state is touched only
  with interrupts off (boot self-tests pre-`sti`; ring-3 syscalls run IF=0 via SFMASK). No locks on
  that state. SMP is a Phase-4 retrofit.

## Working method (this project's proven rhythm)
- **Increment → verify in QEMU → adversarially review (for subtle code) → fix → commit → update
  docs.** Don't claim done without a boot-log proof.
- For **subtle/novel** pieces (INC7's DMA mapping/Extent MAP, future crash-consistency work), use a
  **judged design panel** then an **adversarial review panel** via the Workflow tool (find → verify
  with 3 skeptics per finding; majority confirms). The Phase 2/3 panels caught real bugs (a
  GS-neutrality unsoundness in the block/wake switch, a cap-transfer error-swallow, a virtio
  timeout-desync). For **mechanical** pieces (e.g. PCI enum), implement directly + boot-verify.
- Keep the build warning-clean of NEW warnings (the ~8 existing are intentional forward-scaffolding
  dead-code). Don't add dead code; `#[allow(dead_code)]` only for genuine forward state, documented.

## Collaboration model
**Grok** (xAI CLI at `C:\Users\danie\.grok\bin\grok.exe`) writes greenfield Rust:
`& "$env:USERPROFILE\.grok\bin\grok.exe" --prompt-file <path> --cwd "$env:USERPROFILE\Desktop\Cairn"
--always-approve --permission-mode bypassPermissions --disable-web-search --max-turns N`
(model `grok-build`; needs `--max-turns >= 8`; no `--effort`). **Claude** orchestrates, integrates
across files, reviews Grok's `unsafe`, drives the build/boot/verify loop, keeps cap-core's proofs
green, and (later) the real-hardware loop + management-plane UI. Recent Phase 2/3 work was
Claude-led with design/review panels; bring Grok in for large self-contained greenfield (e.g. the
full object-store namespace, or a from-scratch driver).

## The big picture (so you know why)
6 phases: 0 Foundations ✅ · 1 keystone core ✅ · 2 Domains & scheduling ✅ · **3 Zero-kernel I/O +
object store (HERE)** · 4 Real iron (network-boot onto the HPE ProLiant; SMP/ACPI/NUMA; the big
SMP retrofit — see `studio-server-access` memory) · 5 Confidential boot + the beautiful
management plane + TCB verification. Thesis: *everything is a capability over one persistent
substrate, and the kernel gets out of the data path.* Phase 3 makes that real (Extent caps over a
log-structured store; DeviceQueue caps for kernel-free I/O at INC7).

Start by reading `RESUME.md`, confirming the clean boot, then implementing INC7. Ask me nothing you
can answer from the repo — but do confirm direction before any large multi-agent workflow run.
