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
- **DeviceQueue**: `DQ_MAP`→MAP (**first live use of `Rights::MAP`**; maps rings + a USER
  no-cache doorbell page into a trusted driver domain), `DQ_SUBMIT`→WRITE (kernel-mediated
  fallback), `DQ_INFO`→READ.
- New method-id consts live in `capspace.rs` (NOT cap-core): `X_READ=1,X_WRITE=2,X_COMMIT=3`;
  `DQ_INFO=1,DQ_MAP=2,DQ_SUBMIT=3`. Dispatch reuses the existing two-step (verified
  `CapTable::invoke`+`lookup`, then the method-specific `extra`-right match in `dispatch_method`).
- **Payload lives kernel-side** (ObjectMeta is frozen) in const-init static-mut side-tables
  mirroring `NOTIFY`/`ENDPOINTS`: `EXTENTS:[ExtentMeta{lba,len,hash};N]`,
  `DEVQUEUES:[DqMeta{…};N]`, accessed only IRQs-off via `addr_of_mut!` (const-init avoids the
  boot-stack-overflow memcpy cascade). Constructors mirror `create_endpoint`/`mint_timeslice`.
  `reap_domain` already revokes a dead driver domain's DeviceQueue cap (clears its table).

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
back to the previous-good root (lose only the uncommitted batch). The durable name is the on-disk
`content_hash`; at boot `recover()` re-mints Extent caps from the committed root (CPtrs are
ephemeral — sealed sparse 128-bit persisted tokens are CAP_ABI §7, deferred).

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
- **INC5 — append-log store v0 + Extent caps** (`objstore::put`; `X_READ`/`X_WRITE`/`X_COMMIT` arms).
- **INC6 — OBJECTS SURVIVE REBOOT (T2 milestone):** run1 puts+commits, prints hashes; QEMU exits
  (`/root/cairn-disk.img` persists); run2 `recover()` re-mints the root Extent, re-hashes, prints
  the SAME hashes. Two-run wrapper or on-disk boot-count marker.
- **INC7 — DeviceQueue cap + zero-kernel data path (T1 milestone, the namesake):**
  `map_user_mmio_page` (USER+NO_CACHE doorbell) + a map-existing-phys-frame-at-user-VA helper
  (`map_user_page` allocates a fresh frame, can't map the ring frames); `create_device_queue` +
  DEVQUEUES; `DQ_MAP` maps rings+doorbell contiguously into a trusted driver domain (bump
  `MAX_DOMAINS` 5→6), returns one base VA; a ring-3 driver blob fills a descriptor + rings the
  doorbell + polls used with ZERO syscalls; negative test: MAP-masked copy → ErrRights. Optional
  INC8 (may slip to Phase 4): `DQ_SUBMIT` validated descriptors; IRQ completion (IrqHandler +
  Notification); VT-d scaffold.

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
