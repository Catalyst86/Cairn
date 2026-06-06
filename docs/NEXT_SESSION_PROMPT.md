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
  device=0x1042 … virtio-blk`; `virtio-blk: ready (queue0 size=128)`; `virtio-blk: wrote+read LBA8
  512B match=true (flush negotiated=true)`; `objstore: mounted superblock seq=1 …` (or
  `objstore: formatted -> seq=1` on a fresh disk).
- Phase 2 (after the cap self-tests, post-`sti`): the crash-only self-healing loop
  (`domain 4 … terminated: #UD` → `supervisor: RESTARTED …` ×2 → `reaped`), then the endpoint
  rendezvous (`ep: domain2 E_RECV resumed … recv_cptr=3`, MOVE proof `cptr=1 => status=1`), plus
  `perdomain:`/`notify:`/`GRANT_CAP gate`.

## Where we are
- **Phases 0, 1, 2 are COMPLETE.** Phase 2 = ring-3 domains, EDF + time-capabilities, portal IPC
  (blocking endpoint rendezvous + capability transfer + scheduler block/wake), and crash-only
  domain supervision **+ restart/self-healing**.
- **Phase 3 (zero-kernel I/O + object store) is UNDERWAY:** INC1 PCI enum ✅, INC2 virtio-blk read
  ✅, INC3 write ✅, INC4 Cairnlog superblock + content hash + `flush` ✅ (proven to **persist
  across a reboot**: boot1 formats seq=1, boot2 mounts seq=1). Last feature commit: `6a29905`.

## STEP 3 — Your task: Phase 3 INC5 — append-log `put` + content-addressed Extent caps
Everything below is fleshed out in `docs/PHASE3.md` and `RESUME.md`; this is the concrete plan.
In `kernel/src/objstore.rs` (it already has `MOUNTED: Superblock`, `fnv1a`, the A/B superblock
read/write, and `mount()`):
- `pub fn put(bytes: &[u8]) -> Option<(u64 /*lba*/, u32 /*len*/, u64 /*hash*/)>`: starting at
  `MOUNTED.log_head_lba`, write the data sectors, THEN a record header sector
  (`{rec_magic, kind, content_len, content_hash, prev_lba}`) — **data-before-header**, so a torn
  tail is never advertised. `fnv1a` the content. `virtio_blk::flush()`. Then **commit**: write the
  OTHER superblock slot (alternate LBA0/1) with `seq+1`, the new root `(lba, len, hash)`, and an
  advanced `log_head_lba`; `flush()` again. **The superblock flip is the single commit point.**
  Update the in-RAM `MOUNTED`.
- Mint an **Extent cap** naming the content. In `kernel/src/capspace.rs` (mirror the existing
  `NOTIFY`/`ENDPOINTS` pattern EXACTLY — const-static side-table, never touch cap-core):
  - `pub const X_READ: u16 = 1; pub const X_WRITE: u16 = 2; pub const X_COMMIT: u16 = 3;`
  - `static mut EXTENTS: [ExtentMeta; OBJECT_TABLE_SIZE]` where
    `ExtentMeta { lba: u64, len: u32, hash: u64 }`, const-initialized (NOT by-value-on-stack — the
    boot-stack-overflow bug class).
  - `pub fn mint_extent(lba, len, hash) -> Option<u16>`: `create_object(ObjectKind::Extent)` +
    `EXTENTS[oid] = meta` + mint a root cap with `READ|WRITE|MAP|DELEGATE` (mirror
    `create_endpoint`/`create_notification`).
  - Wire dispatch: in `dispatch_method`'s `extra`-rights match add `(Extent, X_READ) => READ` and
    `(Extent, X_WRITE | X_COMMIT) => WRITE`; add `(Extent, X_READ)` arm returning the **metadata**
    `{lba, len, hash}` (X_READ returns metadata ONLY — bytes reach a domain via Extent **MAP**,
    which ships with INC7, per CAP_ABI §5 "core not in the bulk data path"). `X_WRITE`/`X_COMMIT`
    drive `objstore::put`/commit.
- **Proof (boot-log):** `objstore::put` some bytes (kernel self-test), `cap_invoke(extent, X_READ)`
  returns the metadata, then re-read the on-disk bytes and confirm `fnv1a == hash`. Print it.

Then **INC6** (objects survive reboot — on `mount`, re-mint the root Extent from the committed
superblock; `/root/cairn-disk.img` persists across runs; prove with a 2-run or boot-count marker)
and **INC7** (zero-kernel DeviceQueue grant + Extent MAP — the namesake pillar-4 milestone, bumps
`MAX_DOMAINS` 5→6).

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
- For **subtle/novel** pieces (INC5's crash-consistent commit, INC7's DMA mapping/Extent MAP), use a
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

Start by reading `RESUME.md`, confirming the clean boot, then implementing INC5. Ask me nothing you
can answer from the repo — but do confirm direction before any large multi-agent workflow run.
