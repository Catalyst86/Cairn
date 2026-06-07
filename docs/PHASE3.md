# Phase 3 — Zero-kernel I/O + persistence (object store)

Two theses must both land: **T1** the kernel leaves the I/O hot path after a one-time cap
grant (DESIGN.md pillar 4), and **T2** objects survive reboot (pillar 5). Build order:
reach **T2 on a kernel-side polled driver first** (lowest risk to the milestone), then
deliver **T1** as the DeviceQueue zero-kernel grant. Designed via a judged 3-way panel.
cap-core stays **byte-unchanged** throughout (`git diff HEAD -- crates/` empty is the gate).

## Layers (top = persistence, bottom = bytes)
- **L4 Extent cap** (`ObjectKind::Extent = 8`) — names an immutable, content-addressed extent.
- **L3 Cairnlog store** (`objstore.rs`) — content-addressed, CoW, Merkle-checksummed append
  log + an atomic A/B superblock.
- **L2 block layer** — `read_sector`/`write_sector(lba, &[u8;512])` + `flush`.
- **L1 virtio-blk driver** (`virtio_blk.rs`) — one split virtqueue, polled.
- **L0 PCI enumeration** (`pci.rs`) — find vendor `0x1AF4` / class `0x01`, decode BARs. ✅ DONE.

## Cap-model mapping (cap-core UNCHANGED — every hook already exists)
`ObjectKind::Extent=8`, `DeviceQueue=9` (and `IrqHandler=11`) are defined+unused; `Rights`
already has `MAP`/`READ`/`WRITE`/`DELEGATE`/`REVOKE`. **No new cap-core anything.**
- **Extent**: `X_READ`→READ (returns metadata `{lba,len,hash}` only — bytes reach a domain
  via Extent **MAP**, never a register; CAP_ABI §5 "core not in the bulk data path"),
  `X_WRITE`→WRITE (mints a NEW Extent — content-addressed CoW), `X_COMMIT`→WRITE.
- **DeviceQueue** (INC7 ✅ DONE): `DQ_MAP`→MAP (**first live use of `Rights::MAP`**; maps the
  rings + buffer + a USER no-cache doorbell page into a trusted driver domain), `DQ_INFO`→READ
  (echoes qsize), `DQ_REPORT`→WRITE (the **v0 proof channel** — a ring-3 driver reports its
  zero-syscall I/O result; the kernel validates + logs it). The originally-planned `DQ_SUBMIT`
  (kernel-mediated/validated descriptor submit) is the **deferred v1 escalation**, not v0.
- New method-id consts live in `capspace.rs` (NOT cap-core): `X_READ=1,X_WRITE=2,X_COMMIT=3`;
  `DQ_INFO=1,DQ_MAP=2,DQ_REPORT=3`. Dispatch reuses the existing two-step (verified
  `CapTable::invoke`+`lookup`, then the method-specific `extra`-right match in `dispatch_method`).
- **Payload lives kernel-side** (ObjectMeta is frozen) in const-init static-mut side-tables
  mirroring `NOTIFY`/`ENDPOINTS`: `EXTENTS:[ExtentMeta{lba,len,hash};N]`,
  `DEVQUEUES:[DqMeta{…};N]` (ring/buffer/doorbell raw guest-phys + qsize + proof state), accessed
  only IRQs-off via `addr_of_mut!` (const-init avoids the boot-stack-overflow memcpy cascade).
  Constructors mirror `create_endpoint`/`mint_timeslice`. **`reap_domain` revokes a dead driver
  domain's DeviceQueue cap (clears its table) but does NOT unmap the `DQ_MAP`-ed pages** — an
  accepted v0 leak (single shared address space, no IOMMU): the mapped ring outlives the domain
  until a per-domain mapping ledger + queue reset land (INC7b/later).

## DMA trust boundary (HONEST — this kernel has NO IOMMU)
A virtqueue descriptor's `addr` is a raw guest-physical address the device DMAs to/from. With
no VT-d, any domain that can write descriptors can name **any** frame (guest-phys =
`frame*4096`, trivially derivable) — a write-anywhere primitive, **strictly worse than the
M_FREE gap**. So v0:
- The **kernel-side polled driver** programs every descriptor (the T2 persistence path).
- For the **T1 zero-kernel path**, `DQ_MAP` is granted ONLY to a **trusted driver domain**
  (TCB-adjacent, the SPDK/DPDK posture) — isolated by per-domain CapTables + Rust + crash-only
  restart, but NOT contained against malicious DMA.
- Untrusted application domains get only **Extent caps** (metadata READ + MAP-to-read-bytes);
  they never touch a ring.
- Escalation ladder (documented limitation, like M_FREE): v0 trusted domain → v1 `DQ_SUBMIT`
  kernel-validated descriptors (kernel checks each addr ∈ frames the DeviceQueue owns; one
  crossing per batch, safe but defeats zero-kernel) → v2 VT-d per-domain DMA domain (the real
  fix; QEMU q35 can model `intel-iommu`).

## Crash-consistency (Cairnlog)
512B sectors. **LBA0/1 = double-buffered superblock A/B** `{magic,version,seq:u64,log_head_lba,
root_lba,root_len,root_hash[32],sb_hash[32]}`. LBA2.. = append log of `{rec_magic,kind,
content_len,content_hash[32],prev_lba}` + data sectors. **Write path:** hash bytes → write data
sectors THEN the record header (data-before-header so a torn tail is never advertised) → issue
`VIRTIO_BLK_T_FLUSH` and wait (negotiate `VIRTIO_BLK_F_FLUSH`; serialized completion is ordering
not durability under QEMU writeback) → write the OTHER superblock slot `seq+1` + new root +
`sb_hash` → FLUSH again. **The superblock flip is the single linearization point.** **Recovery:**
read A+B, validate `sb_hash`, pick the higher VALID seq; a torn superblock fails its hash → fall
back to the previous-good root (lose only the uncommitted batch). **Read-error vs. invalid (INC5
review fix):** `read_superblock` returns `Io | Invalid | Valid` — a device-level read FAILURE is
NOT the same as a slot that read OK but is fresh/torn. `mount` formats (writing slot A) ONLY when
BOTH slots read OK and are `Invalid`; if EITHER read returned `Io`, that slot may still hold the
latest committed superblock, so `mount` REFUSES to format (leaving the store unmounted) rather than
clobbering it. (Collapsing both to a single `None`, as the original code did, let one transient read
glitch on the sole good slot trigger a reformat that destroyed all committed data.) The durable name
is the on-disk `content_hash`; at boot `recover()` re-mints Extent caps from the committed root (INC6;
CPtrs are ephemeral — sealed sparse 128-bit persisted tokens are CAP_ABI §7, deferred).

## Increment roadmap
- **INC1 — PCI enumeration ✅ DONE** (`pci.rs`, commit pending): legacy `0xCF8/0xCFC` scan of
  bus 0, BAR decode/size (32/64-bit + I/O), flags the virtio-blk. Verified: lists host bridge,
  VGA, e1000, **virtio-blk `0x1af4:0x1042`** (bar1 mmio32 0x1000, bar4 mmio64 0x4000), AHCI, SMBus.
- **INC2 — virtio-blk MVP (kernel-side, polled) ✅ DONE** (`virtio_blk.rs`, `pci.rs` helpers,
  `paging::map_mmio_range`; commit `fc748d5`). Reads LBA0 via DMA → magic match. `read_sector(lba,
  &mut [u8;512])` is the reusable L2 primitive. Review-hardened (timeout→device-disable + exact
  completion target; qsize ring indexing; u16 cap offsets; bounded reset). Original plan:
  `map_mmio_range` (idempotent, translate-first
  like `map_one_page` — `map_mmio` is single-page and false-fails on already-mapped; virtio cfg
  windows share pages); walk the virtio PCI capability list (cap id 0x09, cfg_type 1/2/3/4 →
  bar+offset+len, notify_off_multiplier); negotiate modern virtio-1.0 (reset→ACK→DRIVER→
  VIRTIO_F_VERSION_1→FEATURES_OK; also VIRTIO_BLK_F_FLUSH); one split virtqueue from
  `allocate_frame`'d frames (program queue addrs as RAW guest-phys=`frame*4096`, touch rings via
  HHDM — the #1 footgun); 3-desc `VIRTIO_BLK_T_IN` read of LBA0; poll used.idx with an apic-style
  anti-hang guard. **Proof:** print the pre-seeded sector-0 magic `CAIRN-DISK-SECTOR-0-MAGIC-v0`.
- **INC3 — one-sector WRITE round-trip ✅ DONE** (commit `b782c2f`): `write_sector` via a shared
  `submit()`; `wrote+read LBA8 512B match=true`. L2 block layer (read+write) complete.
- **INC4 — Cairnlog superblock + checksums ✅ DONE** (commit `6a29905`; `objstore.rs`, flush in
  `virtio_blk.rs`): FNV-1a hash, A/B superblock @ LBA0/1 (content-hash validated, higher-valid-seq
  wins), format-on-first-boot, `flush()` (negotiated `VIRTIO_BLK_F_FLUSH`). Verified across 2 boots:
  formats seq=1 then mounts seq=1 (superblock persists). LBA0/1 now store-owned (INC2 magic gone).
- **INC5 — append-log store v0 + Extent caps ✅ DONE** (commit `a082d63`; `objstore.rs`,
  `capspace.rs`, `main.rs`, `virtio_blk.rs`). `objstore::put(bytes)` → data sectors → record header
  (data-before-header) → `flush` → flip the OTHER A/B superblock slot (`seq+1`, new root, advanced
  `log_head`) → `flush`; the flip is the single commit point. `extent_content_hash` re-reads + re-hashes
  on-disk bytes. Extent caps (`ObjectKind::Extent=8`, cap-core UNCHANGED): `X_READ`/`X_WRITE`/`X_COMMIT`
  ids + `EXTENTS` const-static side-table (mirror `NOTIFY`/`ENDPOINTS`) + `mint_extent`
  (INVOKE|READ|WRITE|MAP|DELEGATE) + `extent_metadata` (verified INVOKE|READ → `{lba,len,hash}`, the
  INC7 MAP seed); `dispatch_method` gates `X_READ`→READ (returns the content hash) and
  `X_WRITE`/`X_COMMIT`→WRITE (then `ErrMethod` — the bulk write path arrives with Extent MAP in INC7).
  Verified across 5 boots: fresh format seq=1, put lba=2 hash=H content-addressed match=true; persisted
  mounts seq 2→4 (log_head→8) re-put the SAME hash at a fresh lba (CoW append); READ-masked cap
  X_READ⇒ErrRights, X_WRITE⇒ErrMethod. **Adversarial-panel review** (find → 3-skeptic refute; 2 of 5
  confirmed): HIGH — `mount()` conflated a device read error with an invalid slot and could reformat
  over the only good superblock; `read_superblock` now returns `Io|Invalid|Valid`, and `mount` formats
  only when both slots read OK and are fresh/torn (else refuses, leaving the store unmounted). MED — the
  L2 `smoke_test` scratch write moved LBA 8 → 32760 (the growing log reaches LBA 8 by ~boot 4).
- **INC6 — OBJECTS SURVIVE REBOOT (T2 milestone) ✅ DONE** (commit `8e66c0f`; `objstore.rs`,
  `main.rs`). `objstore::recover()` re-derives the committed root from the mounted superblock
  (`{root_lba,root_len,root_hash}`, gated on `root_len>0`) and re-verifies it — re-reads + re-hashes
  the on-disk bytes (`extent_content_hash`) and confirms they still match `root_hash`; a torn root or a
  superblock pointing at corrupt bytes returns `None` (never advertised). Called right after `mount`,
  BEFORE this boot's `put`, so it reflects the PREVIOUS boot's committed root. The boot self-test
  re-mints a live Extent cap from the recovered root (`capspace::mint_extent` — CPtrs are ephemeral,
  re-derived each boot from the durable content hash) and `cap_invoke(X_READ)` confirms the same hash.
  **Verified 2-run on the persisted `/root/cairn-disk.img`:** run1 (fresh) prints `no committed root to
  recover (fresh store)` and commits hash H; run2 prints `recovered root Extent cptr=0 lba=2 len=59
  hash=H (on-disk re-hash matched); X_READ=>Ok …; objects-survive-reboot=true` — run1's object recovered
  as a live cap from disk alone, no new put. (Persisted sealed cap tokens are CAP_ABI §7, deferred.)
- **INC7 — DeviceQueue cap + zero-kernel data path (T1 milestone, the namesake) ✅ DONE**
  (commit `d11de19`; `paging.rs`, `virtio_blk.rs`, `capspace.rs`, `user.rs`, `main.rs`). A trusted
  ring-3 driver (domain 5) does a full virtio-blk READ — descriptor chain, doorbell, used poll —
  with ZERO syscalls, over a granted DeviceQueue cap. `paging::map_user_phys` maps an EXISTING phys
  frame at a USER VA (USER leaf+parents, no alloc/zero, optional NO_CACHE) — the missing primitive
  (`map_user_page` allocates a fresh frame). `virtio_blk` persists the queue-0 ring/doorbell raw
  guest-phys + `device_queue_desc()`. `capspace`: `DqMeta`/`DEVQUEUES`, `create_device_queue`,
  `DQ_INFO=1`/`DQ_MAP=2`/`DQ_REPORT=3`; `DQ_MAP` (first live `Rights::MAP`, gated via the verified
  invoke) maps desc/avail/buf RW, used RO (device DMAs used, not via the CPU PTE), doorbell
  RW+NO_CACHE at `DQ_BASE=0x100_0000`. `user::driver_main` is a 281-byte PIC blob (dual addressing:
  descriptor `addr` fields = raw guest-phys baked into params; derefs via `rbp=DQ_BASE`), bounded
  poll, one `DQ_REPORT`, exit via `ud2`. `MAX_DOMAINS` 5→6 (driver = domain 5; `MAX_TASKS`=5
  unchanged — driver is the 5th task). **Proof (fresh + persisted disk):** `DQ_MAP (first live
  Rights::MAP) => Ok base=0x1000000; MAP-masked copy => ErrRights`; `ring3 driver completed virtio
  READ of LBA 32700 with ZERO syscalls; reported magic=…07 kernel-seeded=…07 match=true` (the
  kernel seeds a magic + clears the shared buffer to a sentinel, so `match=true` requires a real
  device read — the magic is never exposed to the driver). **Designed via a judged 4-way design
  panel** (minimal-T1-first won 44.7/50; key fix: no multi-register ring-3 return ⇒ params baked
  kernel-side, the blob's only syscall is the report) **+ adversarial review** (5 finders → 3-skeptic
  refute; 9 findings, 0 confirmed). Extent MAP deferred to **INC7b**.
- **INC7b — Extent MAP ✅ DONE** (commit `02dc95e`; `objstore.rs`, `capspace.rs`, `main.rs`). Fulfils
  the Extent object's "bytes reach a domain via MAP, never a register" promise (CAP_ABI §5; X_READ
  returned only the hash). `objstore::load_extent(lba,len)` DMAs an extent's sectors into a fresh RAM
  frame (pre-`sti`; extent data lives on disk; v0 single-frame `len≤4096`; frees the frame on a block
  error). `capspace`: `ExtentMeta.data_frame_phys`, `mint_extent_mapped`, `X_MAP=4` +
  `(Extent,X_MAP)=>Rights::MAP` (second live `Rights::MAP`) + `extent_map` (maps the loaded frame
  RO+NX at `EXTENT_MAP_BASE=0x110_0000`, returns the VA). **Proof (fresh + persisted disk):** `extent:
  X_MAP=>Ok va=0x1100000; mapped-bytes hash == committed 0x7b4ded… match=true; MAP-masked
  X_MAP=>ErrRights` — the mapped bytes re-hash to the committed content hash. **Adversarial review**
  (3 finders → 3-skeptic refute; 4 findings, 0 confirmed — single-window X_MAP, the documented
  global-mapping invariant, etc., all accepted v0 limitations). cap-core byte-unchanged. Both Phase-3
  theses (T1+T2) now hold and the Extent + DeviceQueue capability models are complete.
- **NEXT (Phase-3 hardening, optional) — reap teardown + escalation rungs:** `reap_domain` revokes a
  dead driver's caps but does NOT unmap its `DQ_MAP`/`X_MAP` pages (accepted v0 leak) — add a
  per-domain mapping ledger + `unmap`-on-reap + queue-0 reset so a reaped driver can't leave a live
  write-anywhere-DMA mapping (SUBTLE: frame-ownership asymmetry — DeviceQueue frames are device-owned
  so unmap-only; Extent scratch frames are mapping-owned so unmap + `deallocate_frame`). Later:
  `DQ_SUBMIT` kernel-validated descriptors; IRQ completion (`IrqHandler` + `Notification`); VT-d
  scaffold (the real DMA-containment fix). **Or pivot to Phase 4** (real-hardware network-boot +
  SMP/ACPI retrofit) — Phase 3's core is complete.

## QEMU / disk setup (in `C:\WSL\cairn-go-kernel.sh`, NOT the repo)
A persistent **16 MiB raw disk** `/root/cairn-disk.img` (created once via `truncate` — `qemu-img`
is NOT installed; sector 0 pre-seeded `CAIRN-DISK-SECTOR-0-MAGIC-v0` for INC2's read-verify),
attached as `-drive file=$DISK,if=none,format=raw,id=d0 -device
virtio-blk-pci,disable-legacy=on,drive=d0`. `disable-legacy=on` forces the pure-modern device
(`1af4:1042`, MMIO-only config) for a clean INC2 capability walk. The image lives in `/root`
(NOT `iso_root`, which the script `rm -rf`s each run) so it survives across boots — exactly what
INC6 needs. To reset the store: `rm /root/cairn-disk.img` (recreated on next run).

## Key risks (carry forward)
- **No IOMMU** → a mapped DeviceQueue is write-anywhere DMA (see trust boundary). v0 = trust only.
- `map_mmio` is single-page + false-fails on already-mapped → INC2 needs idempotent `map_mmio_range`.
- **DMA-phys vs HHDM-virtual:** queue/descriptor addrs are RAW guest-phys; the kernel touches rings
  via HHDM. Mixing them = silent no-completion. Use the bounded poll guard so a mistake prints a
  timeout, not a 20s hang.
- **FLUSH is mandatory** before the superblock flip (ordering ≠ durability under QEMU writeback).
- `MAX_DOMAINS=5` fully consumed (0=root,1=client,2=server,3=grant-test,4=faulter) → INC7 bumps to 6.
- Cap persistence unsolved in v0 — caps RE-MINTED from on-disk content at boot (CAP_ABI §7).
