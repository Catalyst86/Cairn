# Cairn — Session Resume / Handoff

**Read this first, then `DESIGN.md`, `docs/CAP_ABI.md`, and the `cairn-os` memory.**
Cairn is a from-scratch, capability-based OS for James's HPE ProLiant x86-64 server,
built as a **Claude Code × Grok Build** collaboration. Repo: `C:\Users\danie\Desktop\Cairn`.

## ⏭ Next session — start here
1. **Confirm the clean boot** still works: `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-go-kernel.sh`
   — expect the crash-only **self-healing loop**: `crash-only: admitted faulter …` → `domain 4
   (task 1) terminated: #UD rip=0x420025 — crash-only: kernel survives` → `supervisor: RESTARTED
   domain 4 … (1 restart(s) left)` → terminated → `RESTARTED … (0 left)` → terminated →
   `supervisor: domain 4 reaped — restart budget exhausted`; FOLLOWED BY the full 4b endpoint
   rendezvous (`ep: domain2 E_RECV resumed … recv_cptr=3`, MOVE proof `cptr=1 => status=1`) plus
   the `perdomain:`/`notify:`/`GRANT_CAP gate` lines — **no panic/halt** anywhere. PLUS the Phase 3
   lines (early, right after the heap): `pci …` device list incl. `pci 00:03.0 vendor=0x1af4
   device=0x1042 … virtio-blk`; `virtio-blk: ready (queue0 size=128)`; `virtio-blk: wrote+read
   LBA32760 512B match=true (flush negotiated=true)` (the L2 smoke scratch sector moved OFF the
   growing log — INC5 review fix); `objstore: mounted superblock seq=N …` (or `objstore: formatted
   -> seq=1` on a fresh `/root/cairn-disk.img`); and the **INC5 extent proof** `extent: put lba=L
   len=59 hash=0x7b4ded… ; X_READ=>Ok reply_hash=0x7b4ded… ; meta=Some((L,59,…)); on-disk
   re-read=Some(…); content-addressed match=true` then `extent: READ-masked cap X_READ=>ErrRights …
   X_WRITE=>ErrMethod`; the **INC7b Extent-MAP proof** `extent: X_MAP=>Ok va=0x1100000; mapped-bytes
   hash=… committed=0x7b4ded… match=true; MAP-masked X_MAP=>ErrRights` (the persisted bytes mapped RO
   into a domain, re-hash to the committed content hash); the **INC6 recovery proof** `objstore:
   recovered root Extent cptr=0 lba=L len=59 hash=0x7b4ded… (on-disk re-hash matched); X_READ=>Ok …;
   objects-survive-reboot=true` (or `no committed root to recover (fresh store)` on the very first boot
   after `rm`); and the **INC7
   zero-kernel I/O proof** `devqueue: DQ_MAP (first live Rights::MAP) => Ok base=0x1000000; MAP-masked
   copy DQ_MAP => Some(ErrRights)` then `devqueue: ring3 driver completed virtio READ of LBA 32700 with
   ZERO syscalls; reported magic=0xca1707d0dead0007 kernel-seeded=0xca1707d0dead0007 match=true`,
   followed by `domain 5 (task 4) terminated: #UD …` (the driver's clean v0 exit) then the **INC7c
   reap-teardown proof** `reap: domain 5 torn down — unmapped 5/5 granted page(s) (frames object-owned,
   freed at object-destroy)` (the driver's DQ_MAP pages are unmapped on death — the DMA window is
   closed). The store PERSISTS:
   each boot `seq` grows (1→2→3…), `log_head` advances, the same bytes re-`put` to a fresh `lba` with
   the SAME hash (CoW append), and `recover()` re-mints the PRIOR boot's committed root from disk. (`rm
   /root/cairn-disk.img` to reset the store.) Note the old INC2 "read LBA0 magic" line is GONE —
   LBA0/1 now hold the superblock. (The `perdomain:` line's "root CPtr N" integer varies — recover/INC5
   mint Extent caps first — but always `=> ErrBadCPtr (isolated)`; the value is incidental.)
2. **Phase 3 is UNDERWAY** (zero-kernel I/O + object store; architected in `docs/PHASE3.md`).
   **INC1 (PCI enum) ✅ · INC2 (virtio-blk read) ✅ · INC3 (write) ✅ · INC4 (Cairnlog superblock +
   content hash + flush) ✅ · INC5 (append-log `put` + content-addressed Extent caps) ✅ · INC6
   (objects survive reboot — T2 milestone) ✅ · INC7 (zero-kernel DeviceQueue I/O — T1 milestone, first
   live Rights::MAP) ✅ · INC7b (Extent MAP — persisted bytes into a domain, second live Rights::MAP)
   ✅ · INC7c (reap teardown — unmap a dead domain's grants) ✅ · INC8 (DQ_SUBMIT — kernel-mediated,
   DMA-contained block I/O) ✅.** BOTH Phase-3 theses hold (T1 kernel
   out of the I/O hot path + T2 objects survive reboot), the Extent + DeviceQueue capability models are
   complete, and a reaped driver's DMA mapping is now torn down. The L2 block layer
   (`virtio_blk::read_sector`/`write_sector`/`flush`, one shared `submit()`), the L3 superblock
   (`objstore.rs`: FNV-1a hash, A/B superblock at LBA0/1, format/mount, durable via flush), the **L4
   append-log + Extent caps**, and **reboot recovery** all work; the store PERSISTS across QEMU restarts
   (verified: a committed object is re-minted as a live Extent cap on the next boot from disk alone).
   **INC5 (done, commit `a082d63`):** `objstore::put(bytes)` writes data sectors → a record header
   (data-before-header) → `flush` → flips the OTHER A/B superblock slot (`seq+1`, new root, advanced
   `log_head`) → `flush` (the flip = the single commit). `extent_content_hash` re-reads on-disk bytes
   to verify. `capspace`: `X_READ/X_WRITE/X_COMMIT` ids, `EXTENTS` const-static side-table, `mint_extent`
   (mints INVOKE|READ|WRITE|MAP|DELEGATE), `extent_metadata` (verified INVOKE|READ → full
   `{lba,len,hash}`, the INC7 MAP seed); `dispatch_method` gates X_READ→READ (returns the content
   hash) and X_WRITE/X_COMMIT→WRITE (then ErrMethod — bulk write path is INC7). Two adversarial-panel
   fixes folded in: (HIGH) `mount()` no longer reformats over a slot whose READ failed at the device
   level (`read_superblock` now returns `Io|Invalid|Valid`; format only when both slots read OK and
   are fresh/torn); (MED) the L2 smoke scratch write moved LBA 8 → 32760 so it never collides with the
   growing log.
   **INC6 (done, commit `8e66c0f`):** `objstore::recover()` re-derives the committed root from the
   mounted superblock (`{root_lba,root_len,root_hash}`, gated on `root_len>0`) and re-verifies it
   (re-reads + re-hashes the on-disk bytes; mismatch ⇒ `None`, never advertises a corrupt root). The
   boot self-test re-mints a live Extent cap from it (`mint_extent`) and `cap_invoke(X_READ)` confirms
   the same hash. Verified 2-run: run1 fresh ⇒ "no committed root"; run2 recovers run1's object
   `objects-survive-reboot=true` WITHOUT a new put. (CPtrs are ephemeral, re-derived each boot from the
   durable content hash; sealed persisted tokens are CAP_ABI §7, deferred.)
   **INC7 (done, commit `d11de19`):** a trusted ring-3 driver (domain 5) does a full virtio-blk READ
   with ZERO syscalls over a granted DeviceQueue cap. `paging::map_user_phys` maps an EXISTING phys
   frame at a USER VA (USER leaf+parents, no alloc/zero, optional NO_CACHE). `virtio_blk` persists the
   ring/doorbell raw guest-phys + `device_queue_desc()`. `capspace`: `DqMeta`/`DEVQUEUES`,
   `create_device_queue`, `DQ_INFO=1`/`DQ_MAP=2` (first live `Rights::MAP`, gated via the verified
   invoke)/`DQ_REPORT=3` (v0 proof channel; `DQ_SUBMIT` kernel-validated submit deferred to v1);
   `MAX_DOMAINS` 5→6. `user::driver_main` = a 281-byte PIC blob (dual addressing: descriptor `addr` =
   raw guest-phys baked into params; derefs via `rbp=DQ_BASE`), bounded poll, one `DQ_REPORT`, `ud2`
   exit. Designed via a judged 4-way design panel (minimal-T1-first, 44.7/50) + adversarial review
   (9 findings, 0 confirmed). Trust boundary (HONEST v0): no IOMMU + one shared address space ⇒ a
   mapped writable ring is write-anywhere DMA + reachable by every ring-3 task; trusted-domain-only
   is enforced by capability distribution, not the MMU. (INC7c then added unmap-on-reap so a dead
   driver's DQ_MAP pages no longer linger.)
   **INC7b (done, commit `02dc95e`):** `objstore::load_extent` DMAs a committed extent's sectors into a
   fresh RAM frame (pre-`sti`; v0 single-frame ≤4096B; frees the frame on a block error);
   `capspace`: `ExtentMeta.data_frame_phys`, `mint_extent_mapped`, `X_MAP=4` +
   `(Extent,X_MAP)=>Rights::MAP` + `extent_map` (maps the frame RO+NX at `EXTENT_MAP_BASE=0x110_0000`).
   The boot self-test reads the mapped bytes and confirms `fnv1a == the committed hash`
   (`X_MAP=>Ok va=0x1100000 … match=true`; MAP-masked ⇒ ErrRights). Adversarial review (3 finders →
   3-skeptic; 4 findings, 0 confirmed). Fulfils the Extent "bytes via MAP, never a register" promise.
   **INC7c (done, commit `82980f9`):** per-domain mapping ledger (`DOMAIN_MAPS`) records each
   `DQ_MAP`/`X_MAP` USER VA under the invoking domain (`domain` threaded through `dispatch_method`);
   `reap_domain` UNMAPS them when the domain dies — closing the lingering write-anywhere-DMA window.
   UNMAP-ONLY: frames are object-owned (live virtio rings; an extent's pinned data frame), freed at
   object-destroy (deferred), not on mapping-domain death. Proof: driver (domain 5) exits → `reap:
   domain 5 torn down — unmapped 5/5 granted page(s)`. Adversarial review (5 findings, 1 confirmed):
   the draft wrongly tagged the Extent frame mapping-owned + freed it on reap (latent double-free/UAF
   — it is object-owned); FIXED by unmap-only.
   **INC8 (done, commit `316089f`):** `DQ_SUBMIT=4` (needs INVOKE|WRITE) — the DMA-trust escalation
   ladder's v1 rung + completes the DeviceQueue method set. The kernel reads a block into its OWN
   buffer (never a domain-named frame) and returns the content hash, so an UNTRUSTED domain (a
   DeviceQueue cap with WRITE but no MAP) gets safe block I/O without `DQ_MAP`'s write-anywhere-DMA.
   The same MAP-less cap is refused `DQ_MAP` (ErrRights) but allowed `DQ_SUBMIT` (Ok + hash). v0:
   single queue 0 (demo pre-`sti`); per-queue isolation for concurrent runtime use needs multi-queue.
   **frame-alloc Kani (done, commit `84093f4`):** the 4 frame-alloc proofs now PASS (were hanging
   CBMC at FRAMES=128). Fix = FRAMES 128→8 + `#[kani::unwind(10)]` in the proof harness only
   (`crates/frame-alloc/src/verification.rs`); allocator logic + cap-core byte-unchanged. Re-run via
   `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/frame-alloc-kani.sh` (0.22s, "VERIFICATION:- SUCCESSFUL").
   **NEXT (optional, larger) = IRQ-driven I/O OR Phase 4.** IRQ completion (`IrqHandler` +
   `Notification` instead of polling — needs IOAPIC/MSI-X + a blocking `N_WAIT`); VT-d/`intel-iommu`
   scaffold (real per-domain DMA containment, best validated on real HW); object-destroy + refcounted
   frame reclamation (the deferred frame-free path). **Or pivot to Phase 4** (network-boot onto the
   real HPE ProLiant; the big SMP/ACPI/NUMA retrofit — needs the server). Phase 3's core + the v1
   escalation rung + both verified crates' Kani proofs are complete. cap-core byte-unchanged.
   - **Or small Phase-2 polish** (`docs/CRASH_ONLY.md`/`PORTAL_IPC.md` "deferred"):
     survivor-liveness scrub, per-domain frame reclamation on death, blocking `N_WAIT`.
3. **cap-core (`crates/cap-core`) stays byte-unchanged** (the regression gate; `git diff HEAD --
   crates/cap-core/` must be empty — the verified core is frozen). NOTE: `frame-alloc`'s harness was
   fixed this session (it is NOT frozen), so `git diff HEAD -- crates/` is no longer the gate — use
   the `cap-core` path. Kani: BOTH verified crates' proofs now pass — cap-core's 4 (re-confirm:
   `cargo kani -p cap-core --features kani`, ~26 min) and frame-alloc's 4 (re-confirm:
   `bash /mnt/c/WSL/frame-alloc-kani.sh`, ~0.2s after the FRAMES=8 + unwind fix). `kani-proofs.sh`
   ran cap-core then frame-alloc; the frame-alloc HANG is FIXED. Reap stale solvers by NAME first
   (`pkill -9 cbmc`, NOT `-f` which self-matches). When invoking Kani via the PowerShell tool, use a
   SCRIPT FILE (inline `bash -c` mangles `$HOME`/quotes + lacks `~/.cargo/bin` on PATH). At handoff
   the last FEATURE commit is `84093f4` (frame-alloc Kani fix; INC8 DQ_SUBMIT is `316089f`). HEAD is
   the RESUME-update commit after them; run `git log --oneline -25`.

## Status (Phase 3: zero-kernel I/O + object store — UNDERWAY) 🚧
- ✅ **INC1 — PCI bus enumeration** (commit `4218ace`; `pci.rs`, `docs/PHASE3.md`). Architected
  via a judged 3-way design panel. Legacy `0xCF8/0xCFC` config access (q35 root bus); `cfg_read32`
  /`cfg_write32`, BAR decode+size (32/64-bit + I/O via the all-ones probe, IRQs off), `scan()`
  over bus 0. **Verified in QEMU:** host bridge (0x8086:0x29c0), VGA, e1000, **virtio-blk modern
  `0x1af4:0x1042`** (bar4 mmio64 @0xfe000000 size 0x4000 — the cfg window INC2 maps), AHCI, SMBus;
  correct BAR sizes; Phase-2 boot intact; no faults. The QEMU virtio-blk disk + `disable-legacy=on`
  are wired in `C:\WSL\cairn-go-kernel.sh` (persistent `/root/cairn-disk.img`, magic sector 0).
- ✅ **INC2 — polled virtio-blk driver** (commit `fc748d5`; `virtio_blk.rs`). Modern virtio-1.0:
  PCI cap walk → map cfg windows (`paging::map_mmio_range`) → negotiate VERSION_1 → one split
  virtqueue → polled `VIRTIO_BLK_T_IN`. Reads sector 0 via DMA: `read LBA0 magic="CAIRN-DISK-
  SECTOR-0-MAGIC-v0" match=true`. `pci.rs` gained pub cfg helpers + `find_virtio_blk`/`bar_base`/
  `enable_bus_master`. Focused review (4 fixed: HIGH timeout-stale-state → device-disable +
  exact completion target; qsize ring indexing; u16 cap offsets; bounded reset). `read_sector` is
  the reusable L2 primitive. DMA = raw guest-phys, rings via HHDM.
- ✅ **INC3 — write round-trip** (commit `b782c2f`): `virtio_blk::write_sector` via a shared
  `submit()`; `wrote+read LBA8 512B match=true`. L2 block layer (read+write) complete.
- ✅ **INC4 — Cairnlog superblock + content hash + flush** (commit `6a29905`; `objstore.rs` + flush
  in `virtio_blk.rs`). FNV-1a hash; A/B double-buffered superblock @ LBA0/1 (validated by content
  hash, higher-valid-seq wins); `format`-on-first-boot; `flush()` (negotiated `VIRTIO_BLK_F_FLUSH`,
  no-data `VIRTIO_BLK_T_FLUSH` via parameterized `submit`). **Persists across reboot** (boot1
  formats seq=1, boot2 mounts seq=1 — the foundation for INC6). `MOUNTED`/`fnv1a` are ready for INC5.
- ✅ **INC5 — append-log put + content-addressed Extent caps** (commit `a082d63`; `objstore.rs`,
  `capspace.rs`, `main.rs`, `virtio_blk.rs`). `objstore::put` = data sectors → record header
  (data-before-header) → flush → A/B superblock flip (`seq+1`, new root, advanced `log_head`) →
  flush; the flip is the single commit. `extent_content_hash` re-reads + re-hashes on-disk bytes.
  `capspace`: Extent caps (`ObjectKind::Extent=8`, cap-core UNCHANGED) — `X_READ`/`X_WRITE`/`X_COMMIT`
  ids, `EXTENTS` const-static side-table (mirrors `NOTIFY`/`ENDPOINTS`), `mint_extent`
  (INVOKE|READ|WRITE|MAP|DELEGATE), `extent_metadata` (verified INVOKE|READ → full `{lba,len,hash}`,
  the INC7 MAP seed); `dispatch_method` gates X_READ→READ (returns content hash), X_WRITE/X_COMMIT→
  WRITE (then ErrMethod, bulk path = INC7). **Verified across 5 QEMU boots:** fresh format seq=1, put
  lba=2 hash=H content-addressed match=true; persisted mounts seq 2→3→4 (log_head 4→8), same hash
  re-put to a fresh lba (CoW); READ-masked cap X_READ⇒ErrRights, X_WRITE⇒ErrMethod; recv_cptr=3 + all
  Phase 2 proofs unchanged. **Adversarial-panel review** (judged find → 3-skeptic refute; 2 confirmed
  of 5, 3 correctly dismissed): HIGH — `mount()` conflated a device READ error with an invalid slot
  and could reformat over the only good superblock (total data loss); `read_superblock` now returns
  `Io|Invalid|Valid` and `mount` refuses to format unless BOTH slots read OK and are fresh/torn. MED —
  `smoke_test` scrubbed LBA 8 (the growing log reaches it ~boot 4); moved to LBA 32760. cap-core
  byte-unchanged.
- ✅ **INC6 — objects survive reboot (T2 milestone)** (commit `8e66c0f`; `objstore.rs`, `main.rs`).
  `objstore::recover()` re-derives the committed root from the mounted superblock (gated `root_len>0`)
  and re-verifies it (re-reads + re-hashes the on-disk bytes; mismatch ⇒ `None`, never advertises a
  corrupt root). The boot self-test re-mints a live Extent cap from it (`mint_extent`) and
  `cap_invoke(X_READ)` confirms the same content hash. **Verified 2-run on the persisted disk:** run1
  fresh ⇒ `no committed root to recover (fresh store)`; run2 `recovered root Extent cptr=0 lba=2 …
  objects-survive-reboot=true` — run1's committed object re-minted as a live cap from disk alone, no
  new put. CPtrs are ephemeral (re-derived each boot from the durable hash); sealed persisted tokens
  are CAP_ABI §7, deferred. cap-core byte-unchanged; no new warnings.
- ✅ **INC7 — zero-kernel DeviceQueue I/O (T1 milestone, the namesake; first live `Rights::MAP`)**
  (commit `d11de19`; `paging.rs`, `virtio_blk.rs`, `capspace.rs`, `user.rs`, `main.rs`). A trusted
  ring-3 driver (domain 5) does a full virtio-blk READ — descriptor chain, doorbell, used poll — with
  ZERO syscalls over a granted DeviceQueue cap. `paging::map_user_phys` (map an EXISTING phys frame at
  a USER VA, no alloc/zero, optional NO_CACHE, USER leaf+parents); `virtio_blk` persists ring/doorbell
  raw guest-phys + `device_queue_desc()`; `capspace` `DqMeta`/`DEVQUEUES` + `create_device_queue` +
  `DQ_INFO`/`DQ_MAP`/`DQ_REPORT` (DQ_MAP = first live `Rights::MAP`, via the verified invoke; maps
  desc/avail/buf RW, used RO, doorbell RW+NO_CACHE at `DQ_BASE=0x100_0000`); `MAX_DOMAINS` 5→6.
  `user::driver_main` = a 281-byte PIC blob (dual addressing; bounded poll; one `DQ_REPORT`; `ud2`
  exit). **Verified (fresh + persisted disk):** `DQ_MAP => Ok base=0x1000000; MAP-masked copy =>
  ErrRights`; `ring3 driver completed virtio READ of LBA 32700 with ZERO syscalls … match=true` (the
  kernel seeds a magic + clears the shared buffer to a sentinel, so match=true requires a REAL device
  read — the magic is never exposed to the driver). All prior proofs intact; recv_cptr=3; 8 warnings
  (no new). **Designed via a judged 4-way design panel** (minimal-T1-first, 44.7/50; resolved the
  no-multi-register-return ABI fix: params baked kernel-side) **+ adversarial review** (5 finders →
  3-skeptic refute; 9 findings, 0 confirmed). **Trust boundary (HONEST v0):** no IOMMU + one shared
  address space ⇒ a mapped writable ring is write-anywhere DMA + reachable by every ring-3 task;
  trusted-domain-only via capability distribution, not the MMU. (INC7c later added unmap-on-reap so a
  dead driver's DQ_MAP pages no longer linger.) cap-core byte-unchanged.
- ✅ **INC7b — Extent MAP (persisted bytes into a domain; second live `Rights::MAP`)** (commit
  `02dc95e`; `objstore.rs`, `capspace.rs`, `main.rs`). `objstore::load_extent(lba,len)` DMAs a
  committed extent's sectors into a fresh RAM frame (pre-`sti`; v0 single-frame ≤4096B; frees the frame
  on a block error). `capspace`: `ExtentMeta.data_frame_phys`, `mint_extent_mapped`, `X_MAP=4` +
  `(Extent,X_MAP)=>Rights::MAP` + `extent_map` (maps the frame RO+NX at `EXTENT_MAP_BASE=0x110_0000`,
  returns the VA). **Verified (fresh + persisted disk):** `extent: X_MAP=>Ok va=0x1100000; mapped-bytes
  hash == committed 0x7b4ded… match=true; MAP-masked X_MAP=>ErrRights` — the persisted bytes mapped RO
  into a domain re-hash to the committed content hash, fulfilling the Extent "bytes via MAP" promise.
  Adversarial review (3 finders → 3-skeptic refute; 4 findings, 0 confirmed). Warnings 8→6
  (deallocate_frame now live). cap-core byte-unchanged.
- ✅ **INC7c — reap teardown (unmap a dead domain's grants)** (commit `82980f9`; `capspace.rs`). A
  per-domain mapping ledger (`DOMAIN_MAPS`) records each `DQ_MAP`/`X_MAP` USER VA under the invoking
  domain (`domain` threaded through `dispatch_method`); `reap_domain` UNMAPS them when the domain dies,
  closing the lingering write-anywhere-DMA window. **UNMAP-ONLY:** frames are object-owned (live virtio
  rings; an extent's pinned data frame), reclaimed at object-destroy (deferred), NOT freed on
  mapping-domain death. **Verified:** driver (domain 5) exits → `reap: domain 5 torn down — unmapped
  5/5 granted page(s)`; faulter (no maps) reaps with no teardown line. **Adversarial review** (3 finders
  → 3-skeptic; 5 findings, 1 confirmed): the draft wrongly tagged the Extent frame mapping-owned and
  freed it on reap (latent double-free/UAF — it is object-owned); FIXED by unmap-only. cap-core
  byte-unchanged; 6 warnings (no new).
- ✅ **INC8 — DQ_SUBMIT (kernel-mediated, DMA-contained block I/O)** (commit `316089f`; `capspace.rs`,
  `main.rs`). The DMA-trust escalation ladder's v1 rung + completes the DeviceQueue method set. The
  kernel reads a block into its OWN buffer (never a domain-named frame) and returns the content hash,
  so an UNTRUSTED domain (DeviceQueue WRITE, no MAP) gets safe I/O without `DQ_MAP`'s write-anywhere
  DMA. Same MAP-less cap: `DQ_MAP`⇒ErrRights, `DQ_SUBMIT`⇒Ok+hash. Verified pre-`sti`; 6 warnings.
- ✅ **frame-alloc Kani proofs fixed** (commit `84093f4`; `crates/frame-alloc/src/verification.rs`).
  The 4 proofs (no-double-alloc, distinct, roundtrip, bounds) PASS — were hanging CBMC at FRAMES=128
  (the exhaust loop's ~128-deep unwind). Fix = FRAMES 128→8 + `#[kani::unwind(10)]` (harness only;
  allocator logic + cap-core byte-unchanged). 0.22s, "VERIFICATION:- SUCCESSFUL". Both verified crates
  green. Re-run: `bash /mnt/c/WSL/frame-alloc-kani.sh`.
- 🚧 **NEXT (optional, larger) = IRQ-driven I/O OR Phase 4.** IRQ completion (`IrqHandler` +
  `Notification` instead of polling — needs IOAPIC/MSI-X + blocking `N_WAIT`); VT-d/`intel-iommu`
  scaffold (real per-domain DMA containment, best validated on real HW); object-destroy + refcounted
  frame reclamation. **Or pivot to Phase 4** (real HPE ProLiant network-boot; the big SMP/ACPI/NUMA
  retrofit — needs the server). Phase 3's core + v1 escalation rung + both crates' Kani proofs are
  complete. `docs/PHASE3.md`.

## Status (Phase 2: crash-only domain supervision + restart) ✅
- ✅ **Restart / self-healing** (commit `a1b37bf`; `supervisor.rs`): `terminate_current` calls
  `supervisor::on_domain_death(domain)` (after reap, before pick_next) which, for a registered
  restartable domain with budget left, decrements and **re-admits a fresh instance** (fresh caps
  into the reaped domain table, reused slot, pages persist). Budget 0 ⇒ stays reaped. Re-entrancy
  sound (terminate holds only a raw SCHED ptr; admit_user's transient `&mut` doesn't alias).
  Focused adversarial review: 0 confirmed findings. Verified: faulter `#UD` → RESTARTED (budget
  2→1→0) → reaped, kernel + endpoint demo alive throughout (3 terminations, 2 restarts, 0 faults).
- ✅ **A ring-3 fault terminates just that domain; the kernel + other domains live on**
  (commit `b5362c3`; `docs/CRASH_ONLY.md`). DESIGN.md pillar 8. Implemented directly (it
  extends the 4b block/wake machinery) + put through a 4-dimension adversarial review (2
  findings fixed; the terminate/GS asm drew ZERO).
  - `interrupts.rs`: `faulted_in_ring3` (`code_segment.rpl()==Ring3`) routes ring-3
    `#PF`/`#GP`/`#UD` to `terminate_ring3`; ring-0 faults stay FATAL.
  - `sched.rs::terminate_current`: frees the slot (`num--`), `capspace::reap_domain`, picks the
    earliest-deadline peer, and `jump_to_task`s in — NEVER returning to the fault. `jump_to_task`
    = the restore HALF of `block_and_switch` (mov rsp; pop15; iretq) with **NO swapgs** (a ring-3
    exception is already at GS=user(0); every target expects user(0)). `MAX_TASKS` 4→5.
  - `capspace.rs::reap_domain`: revokes the dead domain's authority (clears its CapTable; shared
    objects persist for other holders) + scrubs `ENDPOINTS[oid]` where the dead task was the
    parked peer. `MAX_DOMAINS` 4→5.
  - **Verified in QEMU:** a faulter domain `M_ALLOC`s then `ud2` → `domain 4 terminated: #UD …
    kernel survives`, then the full 4b endpoint rendezvous completes — fault isolated, no halt.
  - **Review fixes:** HIGH — the kernel-stack-overflow guard check now gates on `!USER_MODE` so a
    ring-3 task reading the guard VA can't masquerade as an overflow and halt the kernel (DoS).
    MED (documented v0 gap) — the endpoint scrub is one-directional (a survivor parked *waiting
    for* a dead task blocks forever; needs peer-tracking/timeouts/restart → 4c).
  - **Deferred (later):** per-domain frame/object reclamation on death (bounded-leak; tied to the
    Memory-CPtr redesign), survivor-liveness scrub (a survivor parked *waiting for* a dead task
    blocks forever — needs peer-tracking/timeouts). cap-core byte-unchanged. (Restart/self-healing
    is DONE — see the block above.)

## Status (Phase 2: blocking endpoint IPC + scheduler block/wake — step 4b) ✅
- ✅ **Synchronous Endpoint IPC across two ring-3 domains, with block/wake + cap-transfer**
  (commit `cffbc81`; `docs/PORTAL_IPC.md`). **Designed via a judged 4-way design panel, then
  put through a 5-dimension adversarial review** (7 findings, all one root cause — cap-transfer
  error/sentinel handling — fixed; the block/wake asm + lost-wakeup logic drew ZERO findings).
  - **Scheduler block/wake** (`sched.rs`): `block_current` parks the running task *inside its
    syscall* and switches to the earliest-deadline peer; `unblock` stages the IPC result then
    clears an INDEPENDENT `blocked` gate (deliver-before-unblock). One new naked routine
    `block_and_switch` saves the blocked task as the **exact same 20-u64 frame** `timer_isr`/
    `build_task_frame_inner` use (resumable by the timer OR a peer's switch via one identical
    pop+iretq tail), resuming at a kernel `resume_point` (kernel CS/SS, IF=0). **GS-neutral**
    paired `swapgs` keeps active GS correct for every incoming task. `pick_next` skips blocked;
    `roll_deadlines` won't re-arm a blocked task. Lost-wakeup-free by IF=0-whole-syscall atomicity.
  - **Endpoint** (`capspace.rs`): `E_SEND`/`E_RECV` single-waiter rendezvous (fast handoff if the
    partner is parked, else block); state in `ENDPOINTS[oid]` (mirrors `NOTIFY`). One scalar
    message word + optional **capability transfer** via the syscall `xfer` slot, two-key gated
    (`GRANT_CAP` channel + `DELEGATE` moved cap), MOVE = verified `delegate(all)` + clear source,
    **atomic with the rendezvous** (a failed grant aborts both, never reports Ok). `reply_cptr`
    returns in rdx via `PER_CPU.reply_cptr@16`. Single `CPTR_NULL` sentinel; `xfer` range-checked.
  - **Verified in QEMU:** server parks on E_RECV → client E_SEND rendezvous wakes it → server
    RESUMES inside the syscall (`recv_cptr=3`) → moved cap works in domain 2 (`M_ALLOC=>Ok`) →
    client's granted-away slot ⇒ `ErrBadCPtr` (MOVE) → `E_SEND` w/o GRANT_CAP ⇒ `ErrRights`. 4a
    proofs intact; no faults. **cap-core byte-unchanged** (4 Kani proofs are the regression gate).

## Status (Phase 2: per-domain CapTables + Notification async IPC — step 4a) ✅
- ✅ **Per-domain CapTables + Notification IPC live** (portal-IPC step 4a; `docs/PORTAL_IPC.md`).
  `capspace.rs` now holds `MAX_DOMAINS` per-domain CapTables (`DOMAINS`, **const static-init**
  to avoid a boot-stack temporary — the original overflow bug class), `CURRENT_DOMAIN` +
  `set_current_domain` (scheduler hook), `cap_invoke_in(domain,..)` routing (the verified
  `CapTable::invoke` is still in the live path, plus a method-specific rights layer), and
  `delegate_from_root` — the **first live use of cap-core's verified `delegate`** (I3:
  child.rights ⊆ parent.rights). New **Notification** object: `N_SIGNAL` (OR bits into
  `NOTIFY[oid]`, needs INVOKE|WRITE) / `N_POLL` (read+clear, needs INVOKE|READ); payload lives
  in capspace because cap-core's `ObjectMeta` is frozen. The **live ring-3 task now runs in its
  OWN domain (1)** with an INVOKE-only Memory cap **delegated** from root (its syscalls use
  `cptr=0` in its own table). `sched.rs` gained `Task.domain`, `admit_user(..,domain,..)`, and a
  per-switch `set_current_domain`. **Verified in QEMU:** `perdomain: domain2 ALLOC via
  delegated(INVOKE-only) cap => Ok; root CPtr 1 in domain2 => ErrBadCPtr` (isolation);
  `notify: domain2 SIGNAL=>Ok POLL(no READ)=>ErrRights; root POLL=>Ok bits=0b101 then 0b0`
  (cross-domain async IPC, rights enforced); ring-3 8× `M_ALLOC` via its delegated cap; no
  faults. **cap-core byte-unchanged** (4 Kani proofs still hold). Claude-led integration.
- ✅ **Ring-3 adversarial review folded in + hardening** (commit `6dda246`). One HIGH, confirmed,
  reachable-from-ring-3 finding fixed: **`M_FREE` arbitrary-frame free** (privesc) — an untrusted
  frame number went straight to `deallocate_frame` with no ownership check; now **refused
  (`ErrMethod`)** until `M_ALLOC` returns a Memory CPtr the kernel can ownership-check (boot-log
  proof `cap_invoke(M_FREE, 0x100) => ErrMethod`). Rest of the review = the Roadmap's documented
  deferrals (raw-frame leak, sysret canonical-RIP guard, IF-during-syscall, NMI paranoid-entry +
  SMEP/SMAP, fair co-scheduling, fatal ring-3 faults). Cosmetic: all 6 `function_casts_as_integer`
  lints rewritten to `fn as *const () as …`; warnings 13→8 (rest are forward-scaffolding dead code).

## Status (Phase 2: ring-3 userspace making real cap_invoke syscalls)
- ✅ **`syscall`/`sysret` + ring 3 + first userspace `cap_invoke`.** `syscall.rs` arms the
  MSRs (STAR/LSTAR/SFMASK/KERNEL_GS_BASE + EFER.SCE|NXE) and holds the naked `syscall_entry`
  stub (swapgs → per-task kernel-stack switch → CAP_ABI↔SysV marshal → `syscall_dispatch` →
  `cap_invoke` → anti-leak zero → `sysretq`). `gdt.rs` reordered for STAR (kCS 0x08, kSS 0x10,
  uDS 0x18, uCS 0x20, TSS 0x28) with a **mutable TSS** whose `rsp0` the scheduler rewrites per
  switch. `sched.rs` gained `admit_user` + ring-3 frames (`build_task_frame_inner`) + per-switch
  `set_kernel_stack`. `paging.rs::map_user_page` maps W^X USER pages; `user.rs` is the ring-3
  blob. **Verified in QEMU:** a CPL-3 task issued **8 real `cap_invoke(M_ALLOC)` syscalls**
  (distinct frames 0x109000…0x110000), each a full ring3→syscall→cap-core→sysret round-trip; a
  revoked TimeSlice cap is still denied admission. Grok built `map_user_page` + `user.rs`; Claude
  built the GDT/MSR/asm/scheduler integration. cap-core byte-unchanged.
## Status (Phase 2: EDF scheduler with time-capabilities running)
- ✅ **EDF scheduler + time-capabilities (DESIGN.md pillar 6).** `sched.rs` now selects
  the earliest-deadline runnable task (`pick_next`) off **calibrated real-time deadlines**;
  `apic.rs` calibrates the APIC timer against PIT channel 2 to a **1 ms tick** (`TICK_NS`).
  Tasks are **admitted only by presenting a live `TimeSlice` capability** (`capspace::{mint_timeslice,
  admit_check,revoke_timeslice}` over *unchanged* verified cap-core — `sched::admit` gates on it).
  Verified in QEMU: `APIC calibrated: ~62k ticks/ms`; 3 periodic tasks (T=2/5/13 ms) admitted;
  a **revoked TimeSlice cap is denied admission (`ErrRevoked`** — live I2); steady CPU share
  **fast 86% / med 10% / slow 3%** (deadline-ordered, ≠ round-robin's ~33/33/33). Built with
  Grok (greenfield calibration + EDF data/policy + cap gate) + Claude (schedule_tick wiring,
  admit, verify). Then **adversarially reviewed** (4-dimension find→verify workflow) and 5
  latent defects fixed: new-task + ISR stack-alignment (SysV RSP%16==8), EDF idle-floor
  starvation, deadline over-spacing for D>T, calibration timeout/fallback, and removal of the
  EDF-incompatible `spawn`. Context-switch frame layout unchanged; the ISR gained one
  `and rsp,-16` alignment instruction.
- ✅ **Preemptive round-robin scheduler.** `kernel/src/sched.rs`: ring-0 kernel tasks
  with per-task stacks, context switch via a **naked timer ISR** (saves full register
  set, calls `schedule_tick` to pick the next task, `iretq`s into it). Boot registers the
  idle boot thread as task 0, spawns demo tasks A + B; the APIC timer round-robins all
  three. Verified: `[task A]`/`[task B]` interleave at ticks 1,4,7,… / 2,5,8,… (gap of 3
  = round-robin over {boot, A, B}), 119+ ticks, no fault. Policy is trivial round-robin —
  EDF deadline-ordering only changes `pick_next` (Grok's lane, next). Single-CPU; SCHED is
  touched only with IRQs off (no lock — a lock would deadlock the ISR vs a preempted holder).
- ✅ **APIC periodic timer (Phase 2 foundation).** `kernel/src/apic.rs` brings up the
  **xAPIC via MMIO** (TCG does NOT emulate x2APIC), masks the legacy PIC, and runs a
  periodic timer on vector `0x20`; the ISR bumps `apic::TICKS` + EOIs. Boot log shows
  `timer tick #1..#5, #100`. The LAPIC MMIO page (`0xFEE00000`) is mapped explicitly
  (`paging::map_mmio`, no-cache) since Limine's HHDM doesn't cover it. Uncalibrated for
  now (raw reload count); real time units arrive with the scheduler.
- ✅ **keystone boots cleanly in QEMU** (Phase 0/1): serial → switch to a **512 KiB
  guard-paged kernel stack** → GDT/IDT → frame allocator → 1 MB kernel heap → #BP
  exception recovery.
- ✅ **Stack-overflow hardening:** `kmain` switches RSP to a dedicated `KERNEL_STACK`
  with an **unmapped guard page**; the #PF handler runs on its **own IST stack** and
  reports `KERNEL STACK OVERFLOW` on a guard hit. Verified by a forced-overflow self-test
  (caught loudly, no silent corruption). This is the durable fix for the bug class below;
  the 1 MiB Limine `StackSizeRequest` now only covers the tiny pre-switch prologue.
- ✅ **cap-core formally verified** — 4 Kani proofs, 343 checks, 0 failures
  (I2 revocation, I3 no-amplification, I4 invoke-requires-right, encode round-trip).
- ✅ **`cap_invoke` is LIVE** (`kernel/src/capspace.rs` driving the verified cap-core).
  Boot log: `init_root => cptr=0`, `cap_invoke(ALLOC,0) => Ok frame=0x100000` then
  `0x101000`, `demo_revoke_then_invoke => ErrRevoked` — the verified I2 epoch-revocation
  invariant running in the real kernel.
- ✅ **The paging "map-then-unmap" bug is FIXED** (root cause + fix below).
- ✅ `frame-alloc` Kani proofs PASS (commit `84093f4`; were hanging at FRAMES=128 — fixed with
  FRAMES=8 + `#[kani::unwind(10)]`). Re-confirm: `bash /mnt/c/WSL/frame-alloc-kani.sh` (~0.2s). cap-core's
  4 proofs also pass: `cargo kani -p cap-core --features kani` (~26 min). Both verified crates green.

## THE PAGING BUG — SOLVED (was: "map-then-unmap")
**The previous diagnosis was WRONG.** It was NOT an unmapped `.bss` tail and NOT
frame-allocator aliasing of a live page table — both were red herrings.

**Real root cause: a kernel-stack overflow into Limine's page tables.** Limine's default
~64 KiB boot stack sits immediately above its own page tables (in QEMU: stack top
~`0x1ff92790`; tables at phys `0x1ff7f000`–`0x1ff82fff`). In debug builds the by-value
construction of the 8 KiB `ObjectTable` static (capspace `OBJECTS`) cascades several
full-struct `memcpy`s (~90 KiB of stack), overflowing the boot stack DOWN through
L1/L2/L3/L4 and zeroing them. The first write to `OBJECTS` (not yet cached in the TLB)
then page-faulted **not-present**, while the CPU kept executing off stale TLB entries —
which is exactly why the #PF handler saw "L4 reads 0": the table genuinely was zero.

**How it was proven (GDB stub, as planned in the old handoff):**
- Normal-context page-table walk: Limine maps ALL of `.bss`; `OBJECTS` was present the
  whole time; **zero frames** were allocated before the fault → both old hypotheses dead.
- QEMU `monitor xp` (physical read, bypasses the broken translation) at the fault: the
  entire page-table region L1–L4 read back as zeros.
- GDB **hardware watchpoint** on the OBJECTS L1 PTE caught the clobber: `compiler_builtins`
  `memcpy`, write dest `0x1ff7f1b8` (inside the tables), RSP `0x1ff7c3f0` (below all four).

**The fix:** request a 1 MiB boot stack via Limine `StackSizeRequest::new(1 MiB)` — a
`.requests` static in `kernel/src/main.rs`; `kmain` reports whether Limine honored it
(`Boot stack: 1024 KiB (Limine request honored)`). Removed `premap_bss` (it was a no-op
built on the false premise). The `#PF` `manual_map` mapper stays only as a defensive
backstop. Building keystone's own page tables is now OPTIONAL polish, not a blocker.

**Debug tooling now in place (reusable, in `C:\WSL\`):** `gdb` installed in WSL1.
- `cairn-gdb.sh [cmds]` — boots the EXISTING `cairn.iso` under a frozen QEMU
  (`-S -gdb tcp::1234`), runs `gdb -batch -x <cmds>` (default `cairn-gdb.cmds`), then
  dumps serial. Reuses the iso so addresses match the last `cairn-go-kernel.sh` run.
- `cairn-gdb.cmds` — break at the #PF handler (by raw address), dump CR2/CR3 + page
  tables physically via `monitor xp`.
- `cairn-gdb-wp.cmds` — `set language c`, then a HW `watch` on a PTE to catch the clobber.
- Gotchas: with the kernel ELF loaded, gdb is in **Rust mode** → `set language c` before
  any C-typed `watch`/`x`; break by **raw address** (`break *0xADDR` from `nm`) since Rust
  symbols are mangled; use `monitor xp /Ngx <phys>` to read tables when translation is dead.

## Dev environment (CRITICAL — WSL1, not WSL2)
- **WSL2 is BROKEN on this Windows host** (VM rootfs extraction hangs). We use **WSL1**:
  Ubuntu 24.04 imported as `--version 1` to `C:\WSL\UbuntuWSL1`. Runs as root. apt works
  (shares Windows networking). Good for Rust + QEMU (TCG, no KVM) + Kani + gdb.
- **Invoke WSL from the PowerShell tool**, e.g. `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/<script>.sh`.
  Do NOT use the Bash tool for this — git-bash mangles `/mnt/c` paths into `C:/Program Files/Git/mnt/...`.
- **PowerShell 5.1 mangles embedded double-quotes** passed to native exes (git, grok).
  Use `--prompt-file` for grok and here-strings WITHOUT `"` for git commit messages.
- Toolchain: rustup nightly + `rust-src`, `qemu-system-x86` 8.2.2, `ovmf`, `xorriso`,
  Kani 0.67 (`cargo kani`), `gdb` 15.1. Limine 9.6.7 binary at `/root/limine`.
- Repo lives at `C:\Users\danie\Desktop\Cairn` (canonical — EDIT HERE). Scripts rsync it
  to `~/cairn` in WSL, excluding `target/` so incremental builds persist.

## Build / boot / verify commands
- **Build + boot (fast loop):** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-go-kernel.sh`
  (rsyncs `kernel/` only, force-relinks, builds Limine BIOS ISO, runs QEMU 20 s, serial →
  `/root/cairn-serial.log` and stdout).
- **GDB debug:** `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/cairn-gdb.sh [/mnt/c/WSL/<cmds>]`
  (reuses the last-built `cairn.iso`; gdb output via `set logging` → `/root/cairn-gdb*.log`).
- **Full rebuild (kernel + crates):** `.../cairn-rebuild.sh` ; **whole pipeline:** `.../cairn-go.sh`.
- **Kani proofs:** cap-core — `wsl.exe -d Ubuntu -- bash /mnt/c/WSL/kani-proofs.sh` (slow, ~26 min;
  runs both crates). frame-alloc ALONE (fast, ~0.2s) — `wsl.exe -d Ubuntu -- bash
  /mnt/c/WSL/frame-alloc-kani.sh`. BOTH crates' proofs now PASS (the old frame-alloc HANG was fixed
  with FRAMES=8 + `#[kani::unwind(10)]`). Reap stale solvers by NAME first (`pkill -9 cbmc` — `-f`
  self-matches). Kani scripts must `export PATH="$HOME/.cargo/bin:$PATH"` (an inline `bash -c` lacks it).
- Filter serial output in PowerShell with `... | Select-String -Pattern "..."` (no `grep`).
- Helper scripts in `C:\WSL\`: cairn-go-kernel.sh, cairn-rebuild.sh, cairn-go.sh,
  kani-proofs.sh, kani-setup.sh, limine-setup.sh, cairn-gdb.sh, cairn-gdb.cmds,
  cairn-gdb-wp.cmds, investigate-fault.sh, nm-sections.sh, api-dump*.sh.

## Kernel build specifics (so you don't rediscover them)
- Target: built-in **`x86_64-unknown-none`** + rustflags in `kernel/.cargo/config.toml`
  (`code-model=kernel`, `relocation-model=static`, `link-arg=-Tlinker.ld`). NOT a custom
  `.json` target (Rust gates those behind `-Zjson-target-spec` and the format keeps changing).
- Needs `#![feature(abi_x86_interrupt)]` (kernel) and `generic_const_exprs` (frame-alloc).
- Deps: `limine = 0.6.3` (0.3/0.4 are YANKED), `x86_64 = 0.15.4`, `spin`, `linked_list_allocator`,
  `bitflags`. limine 0.6 API: `MemmapRequest`, `StackSizeRequest::new(size)`, `.response()`,
  markers at crate root, `memmap::Entry.type_` / `MEMMAP_USABLE`. x86_64 0.15:
  `GlobalDescriptorTable::append`, `Segment` trait for `CS::set_reg`, `Cr2::read()` → Result.
- `linker.ld`: `.got` placed in `.data` BEFORE `.bss`; `.bss` page-aligned and LAST, with
  `__bss_start`/`__bss_end` symbols. Segment now has FileSiz `0xc0` ≪ MemSiz (clean NOBITS
  `.bss` tail) and Limine maps the whole thing — confirmed by page-table walk. Limine base
  revision negotiates to 3 (crate requests 6 → `is_supported()` false; made non-fatal).
  Limine config uses v9 syntax (`/Entry` + `kernel_path: boot():/boot/keystone`). cargo does
  NOT track `linker.ld`, so the boot script `touch`es a source file to force a relink.
- GDT must reload SS/DS/ES to our data segment (Limine leaves stale selectors → #GP on iretq).
- **Boot stack: request it explicitly** via `StackSizeRequest` — Limine's default boot stack
  is small (~64 KiB) and adjacent to its page tables; deep call chains overflow it (see bug above).

## Collaboration model (Claude × Grok)
- **Grok** (xAI CLI at `C:\Users\danie\.grok\bin\grok.exe`) writes greenfield Rust into the
  repo: `& "$env:USERPROFILE\.grok\bin\grok.exe" --prompt-file <path> --cwd "$env:USERPROFILE\Desktop\Cairn" --always-approve --permission-mode bypassPermissions --disable-web-search --max-turns N`.
  Model `grok-build` does NOT support `--effort`; needs `--max-turns >= 8`.
- **Claude** orchestrates, reviews Grok's `unsafe`, drives the build/boot/verify loop and
  (later) the real-hardware loop, keeps proofs green, builds the management-plane UI.

## Roadmap (Phase 2 COMPLETE → Phase 3 next)
APIC timer ✅ → preemptive round-robin scheduler ✅ → EDF policy + time-caps ✅ →
ring 3 + syscall + first userspace cap_invoke ✅ → ring-3 hardening (M_FREE gate) ✅ →
per-domain CapTables + Notification async IPC ✅ (step 4a) → portal IPC endpoints (sync
rendezvous) + scheduler block/wake + cap-transfer + 2nd ring-3 task ✅ (step 4b) →
crash-only domain supervision (ring-3 fault terminates the domain, not the kernel) ✅ →
crash-only restart / self-healing (supervisor re-admits under a budget) ✅ (Phase 2 COMPLETE) →
**Phase 3 — zero-kernel I/O + object store: PCI enum ✅ (INC1) → polled virtio-blk driver ✅
(INC2) → write round-trip ✅ (INC3) → Cairnlog superblock+hash+flush ✅ (INC4) → append-log
put + content-addressed Extent caps ✅ (INC5) → objects-survive-reboot ✅ (INC6, T2) → DeviceQueue
zero-kernel I/O ✅ (INC7, T1, first live Rights::MAP) → Extent MAP ✅ (INC7b, second live MAP) →
reap teardown ✅ (INC7c, unmap-on-reap) → DQ_SUBMIT ✅ (INC8, contained fallback) → frame-alloc Kani
✅ → IRQ-driven I/O OR Phase 4 (NEXT);
see docs/PHASE3.md** → (Phase-3 core complete: T1+T2 hold; Extent + DeviceQueue models; reap teardown)
Ring-3 follow-ups (deferred, see commits): fair co-scheduling (the demo now co-schedules
faulter+client+server across domains and works, but equal EDF deadlines are still broken by
lowest-index — no round-robin among equal deadlines), return a Memory CPtr (not a raw frame) to
ring 3 — which also unblocks the real `M_FREE` ownership check (M_FREE is currently *refused*)
and per-domain frame reclamation on death, sysret canonical-RIP guard once user RIP is
attacker-influenced, re-enable IF mid-syscall for blocking IPC, NMI paranoid-entry + SMEP/SMAP.
Follow-ups deferred from EDF: per-task budget *enforcement* (preempt on overrun; v0 only
accounts), deadline-miss policy beyond finish-late, calibration accuracy on real HW,
admission utilization check (Σ Cᵢ/Tᵢ≤1), and a way to revoke an *already-admitted* task's
TimeSlice without cap-core in the ISR hot path.
Phase 4 (network-boot onto the real HPE ProLiant via James's existing iPXE server; see the
`studio-server-access` memory) → Phase 5 (confidential boot + beautiful management plane).
Keep adding Kani proofs per component (cap-core's 4 + frame-alloc's 4 now both pass). Building
keystone's own page tables is now optional hardening, no longer blocking.

## Server (not needed until Phase 4)
Target hardware is an HPE ProLiant (x86-64, iLO-managed), currently OFF. Connection details — iLO
address, SSH host/user, and keys — are kept OUT of this public repo; see the private
`studio-server-access` memory (local to the dev machine, not committed).
