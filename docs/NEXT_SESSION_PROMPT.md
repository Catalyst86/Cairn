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
  READ-masked cap X_READ=>ErrRights … X_WRITE=>ErrMethod`; the INC7b Extent-MAP proof `extent:
  X_MAP=>Ok va=0x1100000; mapped-bytes hash == committed 0x7b4ded… match=true; MAP-masked
  X_MAP=>ErrRights`; and the INC6 recovery proof `objstore:
  recovered root Extent cptr=0 lba=L … objects-survive-reboot=true` (or `no committed root to recover
  (fresh store)` on the first boot after `rm`); and the INC7 zero-kernel I/O proof `devqueue: DQ_MAP
  (first live Rights::MAP) => Ok base=0x1000000; MAP-masked copy DQ_MAP => Some(ErrRights)` then
  `devqueue: ring3 driver completed virtio READ of LBA 32700 with ZERO syscalls; reported magic=…07
  kernel-seeded=…07 match=true`, then `domain 5 (task 4) terminated: #UD …` (driver's clean v0 exit),
  then the INC7c reap-teardown line `reap: domain 5 torn down — unmapped 5/5 granted page(s) …`.
  The store persists — each boot `seq` grows, the same bytes re-`put` to a fresh `lba` with the SAME
  hash (CoW), and `recover()` re-mints the prior boot's committed root from disk.
- Phase 2 (after the cap self-tests, post-`sti`): the crash-only self-healing loop
  (`domain 4 … terminated: #UD` → `supervisor: RESTARTED …` ×2 → `reaped`), then the endpoint
  rendezvous (`ep: domain2 E_RECV resumed … recv_cptr=3`, MOVE proof `cptr=1 => status=1`), plus
  `perdomain:`/`notify:`/`GRANT_CAP gate`.

## Where we are
- **Phases 0, 1, 2 are COMPLETE.** Phase 2 = ring-3 domains, EDF + time-capabilities, portal IPC
  (blocking endpoint rendezvous + capability transfer + scheduler block/wake), and crash-only
  domain supervision **+ restart/self-healing**.
- **Phase 3 (zero-kernel I/O + object store):** INC1 PCI enum ✅, INC2 virtio-blk read ✅, INC3 write
  ✅, INC4 Cairnlog superblock+hash+flush ✅, INC5 append-log `put` + content-addressed Extent caps ✅
  (panel fixed a mount() reformat-on-read-error data-loss bug + a smoke/log LBA collision), INC6
  objects-survive-reboot ✅ (T2), INC7 zero-kernel DeviceQueue I/O ✅ (T1, first live `Rights::MAP`:
  a ring-3 driver does a full virtio-blk READ with ZERO syscalls over a granted DeviceQueue cap),
  INC7b Extent MAP ✅ (second live `Rights::MAP`: a committed extent's persisted bytes mapped RO into
  a domain, re-hash == committed hash), INC7c reap teardown ✅ (a reaped domain's DQ_MAP/X_MAP pages are
  unmapped — the DMA window is closed; unmap-only, frames are object-owned). Each subtle increment went
  through a design and/or adversarial panel. **BOTH Phase-3 theses hold — T1 (kernel out of the I/O
  path) + T2 (objects survive reboot) — the Extent + DeviceQueue capability models are complete, and
  reap teardown closes the DMA-mapping leak.** Last feature commit: `82980f9`.

## STEP 3 — Your task: optional escalation rungs OR pivot to Phase 4
Phase 3's CORE IS COMPLETE (T1+T2 proven; Extent + DeviceQueue models done; reap teardown done). What
remains is optional hardening + the next phase — **confirm direction with the user** (they may prefer
Phase 4, or a different priority). cap-core stays byte-unchanged. Candidate work:
- **Escalation rungs / DMA containment:** `DQ_SUBMIT` kernel-validated descriptors (the v1 step — the
  kernel checks each descriptor addr ∈ the DeviceQueue's owned frames); IRQ completion (`IrqHandler`
  + `Notification` instead of polling); VT-d/intel-iommu scaffold (the real per-domain DMA-containment
  fix; QEMU q35 can model `intel-iommu`). See `docs/PHASE3.md` + the DMA-trust-boundary escalation ladder.
- **Object lifecycle:** object-destroy/revoke + frame reclamation (the deferred frame-free path — the
  Extent data frame + a destroyed DeviceQueue's rings currently leak for the boot's lifetime).
- **Phase 4 alternative:** network-boot onto James's HPE ProLiant via the existing iPXE server; the
  big SMP/ACPI/NUMA retrofit (per-CPU run queues, real locks, CR3-per-address-space). See the
  `studio-server-access` memory + `docs/DESIGN.md` phases.

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

Start by reading `RESUME.md`, confirming the clean boot, then (Phase-3 core is complete) confirm with
the user whether to do the optional escalation/DMA-containment rungs, the object-lifecycle frame
reclamation, or pivot to Phase 4. Ask me nothing you can answer from the repo — but do confirm
direction before any large multi-agent workflow run.
