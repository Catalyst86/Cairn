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
  `objstore: formatted -> seq=1` on a fresh disk); and the INC5 extent proof `extent: put lba=L
  len=59 hash=0x7b4ded… ; X_READ=>Ok reply_hash=0x7b4ded… ; … content-addressed match=true` then
  `extent: READ-masked cap X_READ=>ErrRights … X_WRITE=>ErrMethod`. The store persists — each boot
  `seq` grows and the same bytes re-`put` to a fresh `lba` with the SAME hash (CoW append).
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
  content-addressed Extent caps ✅ (verified across 5 boots: seq 1→4, same content hash re-`put` to a
  fresh lba each boot = CoW append; adversarial panel caught + fixed a mount() reformat-on-read-error
  data-loss bug and a smoke-test/log LBA collision). Last feature commit: `a082d63`.

## STEP 3 — Your task: Phase 3 INC6 — objects survive reboot (T2 milestone)
INC5 (commit `a082d63`) landed `objstore::put` (append-log + A/B superblock flip commit) and
content-addressed Extent caps (`mint_extent`, `extent_metadata`, `X_READ`/`X_WRITE`/`X_COMMIT`,
`EXTENTS` side-table). The store already PERSISTS: each boot's committed `put` becomes the next
boot's mounted superblock root (`{root_lba, root_len, root_hash}`). INC6 closes the loop: prove a
committed object is **recoverable as a live Extent cap after a reboot, without re-putting it**.
- In `kernel/src/objstore.rs`: add `pub fn recover() -> Option<(u64, u32, u64)>` (or fold into
  `mount`) that, when `MOUNTED_OK && MOUNTED.root_len > 0`, returns the committed root
  `{root_lba, root_len, root_hash}`; re-hash the on-disk bytes with `extent_content_hash(root_lba,
  root_len)` and confirm it equals `root_hash` (a Merkle-style integrity check on the persisted root).
- In `kernel/src/main.rs` (a boot self-test, root domain, pre-`sti`): after `mount`, call `recover`;
  if a root exists, `capspace::mint_extent(root_lba, root_len, root_hash)` to RE-MINT the root Extent
  cap from on-disk state, `cap_invoke(root_ext, X_READ)` to confirm it returns `root_hash`, and print
  `objstore: recovered root Extent cptr=.. lba=.. len=.. hash=.. reverify=true`. (No cap-core change —
  `mint_extent`/`extent_metadata` already exist. CPtrs are ephemeral, re-minted each boot from the
  durable content hash; sealed persisted tokens are CAP_ABI §7, deferred.)
- **Proof (2 runs on the persisted `/root/cairn-disk.img`):** run1 `put`s+commits → prints
  `content-addressed match=true` with hash H at some lba. run2 `mount`+`recover` re-mints the root
  Extent **from run1's committed superblock** (root_lba = run1's data lba) and prints the SAME hash H
  with `reverify=true` — WITHOUT a new put having produced it. (The boot self-test still does its own
  fresh `put`; INC6's new line specifically proves the PRIOR boot's root survived.) Watch for the
  ordering: `recover` reads the root committed by the PREVIOUS boot (this boot's `put` runs after).

Then **INC7** (zero-kernel DeviceQueue grant + Extent MAP — the namesake pillar-4 milestone: first
live use of `Rights::MAP`, maps ring+doorbell into a trusted driver domain, bumps `MAX_DOMAINS` 5→6;
`extent_metadata` is the seed for Extent MAP). Full plan in `docs/PHASE3.md` INC6/INC7.

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

Start by reading `RESUME.md`, confirming the clean boot, then implementing INC6. Ask me nothing you
can answer from the repo — but do confirm direction before any large multi-agent workflow run.
